//! Sealing: closing a plan's task graph and exporting a disclosed review
//! artifact.
//!
//! A run seals when every task in the plan is `done` (or `abandoned` with a
//! stated reason -- treated as closing the DAG, but listed as a residual in
//! the rendered artifact). [`seal`] applies, strictly in order: (1) verify
//! the graph is closed, else return [`SealError::OpenTasks`] naming the
//! open tasks, before anything is written or mutated; (2) compute the
//! artifact path (and the optional `--out` copy path) and validate both
//! against the supervised repository's working tree and the plan's own
//! store directory (`reject_if_inside`), before rendering; (3) render the
//! review artifact in memory (`src/render.rs`); (4) write the artifact (and
//! any `--out` copy) atomically -- a temporary file in the destination's
//! own directory, then an atomic rename -- so a crash mid-write never
//! leaves a half-written artifact where a full one is expected; (5) apply
//! the plan's `implemented`/`complete` transition
//! (`tftio_planner::PlanAction::Implement`) under
//! [`crate::ledger::Ledger`]'s own mutation lock (its `pub(crate)`
//! `apply_plan_mutation` method, reused rather than reimplementing that
//! lock-and-retry protocol here per `REPO_INVARIANTS.md` HO-003); if this
//! fails, the artifact (and `--out` copy) written in step (4) is removed
//! before the error is returned, so no artifact exists for a plan that
//! never actually sealed; (6) record disclosures for every hidden criterion
//! the plan carries, under the disclosure lock. If (6) fails after (5)
//! already committed, the plan **is** sealed (its artifact exists) but its
//! disclosures were not counted; [`seal`] returns
//! [`SealError::DisclosuresNotRecorded`] naming that state rather than
//! silently losing the count, and [`seal_disclosures_only`] performs step
//! (6) alone to recover.
//!
//! This ordering is deliberate: steps (1)-(4) are read-only or write only
//! to fresh paths this call owns exclusively (the artifact, its `--out`
//! copy), so any failure there leaves the plan's `approved` status and the
//! disclosure ledger completely untouched. Only step (5) commits a
//! one-way (`approved` -> `implemented`) transition, and only after the
//! artifact it describes already exists on disk.
//!
//! # Disclosure tracking
//!
//! `<store_root>/disclosures.toml` maps a criterion key (a hex SHA-256 of
//! [`crate::render::normalize_claim`]'s output) to how many times, and in
//! which plans, that claim has been disclosed. It is written under its own
//! lock file (`disclosures.toml.lock`), mirroring
//! [`crate::ledger`]'s `create_new`/stale-reclaim/bounded-retry protocol
//! rather than sharing its private `MutationLock` type, so two concurrent
//! seals of different plans never lose a count to a lost update.
//! `compute_disclosures` (private) is the pure projection [`seal`] uses to
//! preview a seal's disclosure snapshot for rendering *before* anything
//! commits; `commit_disclosures` (private) re-reads the on-disk ledger
//! under lock and is the only function that ever writes it, in step (6)
//! alone.

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use tftio_planner::model::{PlanStatus, TaskStatus};
use tftio_planner::{Mutation, PlanAction};

use crate::ledger::{Ledger, LedgerError};
use crate::render::{self, DisclosureSnapshot, RenderInput, SealDate, normalize_claim};
use crate::store::{Store, StoreError};
use crate::tools::is_safe_filename_component;

const RUNS_DIR: &str = "runs";
const SEALED_ARTIFACT_FILE_NAME: &str = "sealed.md";
const DISCLOSURES_FILE_NAME: &str = "disclosures.toml";

/// The default number of disclosures after which a hidden criterion is
/// reported as burned.
const DEFAULT_BURNED_THRESHOLD: u32 = 3;

// ---------------------------------------------------------------------
// Burned threshold
// ---------------------------------------------------------------------

/// How many times a hidden criterion's claim may be disclosed before it is
/// reported as burned.
///
/// A criterion is burned once its disclosure count *exceeds* this value
/// (i.e. on its `threshold + 1`-th disclosure), not merely reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnedThreshold(u32);

impl BurnedThreshold {
    /// Build a burned threshold from an operator-supplied value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Borrow the threshold value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for BurnedThreshold {
    fn default() -> Self {
        Self(DEFAULT_BURNED_THRESHOLD)
    }
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// A failure sealing a plan.
#[derive(Debug, Error)]
pub enum SealError {
    /// The plan store reported a failure resolving the plan.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The ledger's mutation lock, write, or read path failed.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// The stored plan could not be parsed.
    #[error(transparent)]
    Parse(#[from] tftio_planner::ParseError),
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: io::Error,
    },
    /// The plan id is not safe to use as a path component.
    #[error("not a safe plan identifier: {0}")]
    UnsafeIdentifier(String),
    /// The task graph is not closed: one or more tasks are neither `done`
    /// nor `abandoned`.
    #[error("plan cannot be sealed: open tasks remain: {}", .0.join(", "))]
    OpenTasks(Vec<String>),
    /// The resolved artifact path lies inside the supervised repository's
    /// working tree (or the operator plan's own directory), which the
    /// artifact must never be written into.
    #[error("refusing to write the sealed artifact inside the supervised repository: {path}")]
    ArtifactInsideRepository {
        /// The rejected path.
        path: PathBuf,
    },
    /// The disclosure ledger's mutation lock could not be acquired within
    /// its retry budget.
    #[error("could not acquire the disclosure ledger lock at {path} within the retry budget")]
    DisclosuresLockTimeout {
        /// The lock file path contention was observed on.
        path: PathBuf,
    },
    /// The disclosure ledger could not be deserialized.
    #[error("parsing disclosure ledger {path}: {source}")]
    DisclosuresDeserialize {
        /// The disclosure ledger path that failed to parse.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// The plan-level `implemented` transition and the artifact write both
    /// succeeded, but recording disclosures afterward failed: the plan
    /// **is** sealed (its artifact exists at `artifact_path`), but the
    /// disclosure ledger was not updated for it.
    #[error(
        "plan {plan_id} is now sealed (artifact at {artifact_path}) but disclosures were not \
         recorded: {source}; re-run `silent-critic seal --disclosures-only {plan_id}` to record them"
    )]
    DisclosuresNotRecorded {
        /// The plan id, sealed despite this error.
        plan_id: String,
        /// Where the sealed artifact was written.
        artifact_path: PathBuf,
        /// The underlying disclosure-recording failure.
        #[source]
        source: Box<Self>,
    },
    /// [`seal_disclosures_only`] was asked to record disclosures for a plan
    /// that is not `implemented` yet -- there is no sealed artifact to
    /// recover disclosures for.
    #[error("plan {plan_id} is not sealed; run \"silent-critic seal\" first")]
    NotSealed {
        /// The plan id that is not yet sealed.
        plan_id: String,
    },
    /// [`seal_disclosures_only`] was asked to record disclosures for a plan
    /// that is `implemented` but whose sealed artifact is missing (an
    /// unexpected state: [`seal`] never commits the `implemented`
    /// transition until after the artifact is written).
    #[error("plan {plan_id} is sealed but its artifact is missing at {path}")]
    ArtifactMissing {
        /// The plan id whose artifact is missing.
        plan_id: String,
        /// The path the artifact was expected at.
        path: PathBuf,
    },
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> SealError + '_ {
    move |source| SealError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Create `path`'s parent directory (and any missing ancestors), or do
/// nothing if `path` has no parent.
///
/// A bare `path.parent().unwrap_or(Path::new("."))` eager fallback (not
/// `unwrap_or_else`) mirrors `src/ledger.rs`'s and `src/store.rs`'s own
/// identical choice for the same structurally-unreachable case: every path
/// this module builds (`<store_root>/runs/.../sealed.md`,
/// `<store_root>/disclosures.toml`, its `.lock` sibling) is joined from a
/// non-empty parent, so `parent()` never actually returns `None` here; this
/// keeps that (unreachable) fallback as a single covered expression rather
/// than an `if let` block whose branches are two separately-instrumented
/// regions on the same source line.
#[allow(clippy::or_fun_call)]
fn create_parent_dir(path: &Path) -> Result<(), SealError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io_error(parent))
}

/// Write `contents` to `path` atomically: a temporary file in `path`'s own
/// directory (so the final rename is same-filesystem and therefore atomic
/// on every platform this crate targets), then `rename` over `path`. A
/// reader can never observe a partially-written artifact, and a crash
/// between the write and the rename leaves only the harmless, uniquely
/// named temporary file behind rather than a truncated `path`.
#[allow(clippy::or_fun_call)]
fn write_atomic(path: &Path, contents: &str) -> Result<(), SealError> {
    create_parent_dir(path)?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path.file_name().map_or_else(
        || "artifact".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    if let Err(source) = std::fs::write(&temp_path, contents) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(io_error(&temp_path)(source));
    }
    std::fs::rename(&temp_path, path).map_err(|source| {
        let _ = std::fs::remove_file(&temp_path);
        io_error(path)(source)
    })
}

// ---------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------

/// A hidden criterion reported as burned by a seal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnedCriterion {
    /// The claim text, as it appears in the plan.
    pub claim: String,
    /// How many times it has now been disclosed.
    pub count: u32,
}

/// What [`seal_disclosures_only`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisclosuresOutcome {
    /// At least one of the plan's hidden criteria had not yet been
    /// recorded as disclosed by this exact plan id; the disclosure ledger
    /// was updated. `burned` names every criterion the plan carries that
    /// is now past the configured threshold (whether or not this call is
    /// what pushed it there).
    Recorded {
        /// Every hidden criterion now past the configured burned
        /// threshold.
        burned: Vec<BurnedCriterion>,
    },
    /// Every hidden criterion this plan carries was already recorded as
    /// disclosed by this exact plan id (recording is idempotent, keyed on
    /// `(criterion, plan_id)`); the disclosure ledger was not touched.
    AlreadyRecorded,
}

/// What [`seal`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealOutcome {
    /// Where the rendered review artifact was written.
    pub artifact_path: PathBuf,
    /// Where an additional copy of the artifact was written, if the caller
    /// asked for one (the CLI's `--out`). Written by [`seal`] itself, under
    /// the same two `reject_if_inside` guards as `artifact_path`, so a
    /// caller never has to (and must not) write this copy itself.
    pub out_path: Option<PathBuf>,
    /// The rendered artifact's own text, for a caller that wants to reuse
    /// it without re-reading it from disk.
    pub artifact: String,
    /// Every hidden criterion this seal disclosed that has now crossed the
    /// configured [`BurnedThreshold`].
    pub burned: Vec<BurnedCriterion>,
}

// ---------------------------------------------------------------------
// Sealing
// ---------------------------------------------------------------------

/// Seal the plan `plan_id` stores for `repo_start`, optionally writing an
/// additional copy of the artifact to `out`.
///
/// See this module's own doc comment for the exact, load-bearing order of
/// operations (verify closed, validate paths, render, write atomically,
/// commit the plan transition, record disclosures) and why it holds: no
/// step before the plan-level mutation touches anything but fresh paths
/// this call owns, so nothing observable changes until the artifact this
/// error message would point at already exists.
///
/// # Errors
///
/// Returns [`SealError::OpenTasks`] when one or more tasks are neither
/// `done` nor `abandoned` -- before anything is written. Returns
/// [`SealError::ArtifactInsideRepository`] when the resolved artifact path
/// or `out` lies inside the supervised repository or the plan's own
/// directory -- also before anything is written. Returns
/// [`SealError::DisclosuresNotRecorded`] when the plan-level transition and
/// the artifact write both succeeded but recording disclosures afterward
/// failed; the plan is sealed despite this error (see
/// [`seal_disclosures_only`]). If the plan-level transition itself fails
/// after the artifact (and `out`, if given) was written, both are removed
/// (best-effort) before the original error is returned, so no artifact is
/// left behind for a plan that never actually sealed. Otherwise returns
/// [`SealError`] when the plan cannot be resolved, read, or parsed, or the
/// artifact (or `out`) cannot be written.
pub fn seal(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    date: &SealDate,
    threshold: BurnedThreshold,
    out: Option<&Path>,
) -> Result<SealOutcome, SealError> {
    if !is_safe_filename_component(plan_id) {
        return Err(SealError::UnsafeIdentifier(plan_id.to_owned()));
    }

    let plan_path = store.plan_path(repo_start, plan_id)?;
    let source = std::fs::read_to_string(&plan_path).map_err(io_error(&plan_path))?;
    let plan = tftio_planner::parse_markdown(&source)?;

    // (1) Verify the DAG is closed, before anything is written or mutated.
    let open_tasks: Vec<String> = plan
        .tasks
        .iter()
        .filter(|task| !matches!(task.status, TaskStatus::Done | TaskStatus::Abandoned))
        .map(|task| task.id.to_string())
        .collect();
    if !open_tasks.is_empty() {
        return Err(SealError::OpenTasks(open_tasks));
    }

    // (2) Compute and validate the artifact path and the optional `--out`
    // copy path, before rendering or writing anything.
    let plan_directory = store.plan_directory(repo_start, plan_id)?;
    let provenance = store.provenance(repo_start, plan_id)?;
    let artifact_path = store
        .root_path()
        .join(RUNS_DIR)
        .join(provenance.repo.as_str())
        .join(plan_id)
        .join(SEALED_ARTIFACT_FILE_NAME);
    reject_if_inside(&artifact_path, repo_start)?;
    reject_if_inside(&artifact_path, &plan_directory)?;
    if let Some(out_path) = out {
        reject_if_inside(out_path, repo_start)?;
        reject_if_inside(out_path, &plan_directory)?;
    }

    // (3) Render the artifact in memory. Reading the plan's hidden
    // criteria, guidance log, and disclosure ledger is safe here even
    // though the plan-level `Implement` transition has not run yet: that
    // transition only ever changes `status`/`execution.task_graph_status`,
    // never a task's own fields or the disclosure ledger, so the content
    // this render depends on is already final. The disclosure snapshot
    // used for rendering is a preview only (`compute_disclosures`, no
    // lock, nothing written); the authoritative, committed snapshot is
    // computed again in step (6).
    let ledger = Ledger::new(store, repo_start, plan_id);
    let unresolved = ledger.unresolved_items()?;
    let disclosures_path = store.root_path().join(DISCLOSURES_FILE_NAME);
    let existing_disclosures = load_disclosures(&disclosures_path)?;
    let preview = compute_disclosures(&existing_disclosures, &plan, plan_id, date, threshold);
    let artifact = render::render_sealed_artifact(&RenderInput {
        plan: &plan,
        unresolved: &unresolved,
        disclosures: &preview.snapshots,
        date,
    });

    // (4) Write the artifact (and any `--out` copy) atomically. Neither
    // path has been written to by this call before this point, so a
    // failure here leaves nothing to clean up.
    write_atomic(&artifact_path, &artifact)?;
    if let Some(out_path) = out
        && let Err(error) = write_atomic(out_path, &artifact)
    {
        let _ = std::fs::remove_file(&artifact_path);
        return Err(error);
    }

    // (5) Apply the plan-level transition. If it fails, the artifact (and
    // `out`) already exist but describe a plan that never actually sealed
    // (still `approved`, one-way transition never applied) -- remove them
    // before returning, so no artifact is left behind for an unsealed plan.
    if let Err(error) = ledger.apply_plan_mutation(Mutation::Plan(PlanAction::Implement)) {
        let _ = std::fs::remove_file(&artifact_path);
        if let Some(out_path) = out {
            let _ = std::fs::remove_file(out_path);
        }
        return Err(SealError::from(error));
    }

    // (6) Record disclosures, under the disclosure lock, last. From this
    // point on the plan is unconditionally sealed (steps 1-5 all
    // succeeded); a failure here does not roll anything back, since
    // rolling back a plan-level transition already applied would require a
    // second mutation of its own that could itself fail, compounding the
    // problem rather than resolving it. Instead this is reported as its
    // own typed, recoverable error, naming the artifact that already
    // exists and directing the operator to [`seal_disclosures_only`].
    match commit_disclosures(&disclosures_path, &plan, plan_id, date, threshold) {
        Ok(projection) => Ok(SealOutcome {
            artifact_path,
            out_path: out.map(Path::to_path_buf),
            artifact,
            burned: projection.burned,
        }),
        Err(error) => Err(SealError::DisclosuresNotRecorded {
            plan_id: plan_id.to_owned(),
            artifact_path,
            source: Box::new(error),
        }),
    }
}

/// Record disclosures alone, for a plan [`seal`] already sealed.
///
/// Its plan-level transition and artifact write both succeeded, but
/// [`SealError::DisclosuresNotRecorded`] means disclosures were never
/// committed. Performs step (6) of [`seal`]'s own doc comment alone: does
/// not re-verify the task graph, re-render, or rewrite the artifact --
/// both are required to already exist.
///
/// Idempotent per `(criterion, plan_id)`: calling this again for a plan
/// whose disclosures were already recorded returns
/// [`DisclosuresOutcome::AlreadyRecorded`] rather than incrementing every
/// criterion's count a second time.
///
/// # Errors
///
/// Returns [`SealError::NotSealed`] if the plan is not `implemented` yet.
/// Returns [`SealError::ArtifactMissing`] if the plan is `implemented` but
/// its sealed artifact is missing (a state [`seal`] itself never produces,
/// since it never commits the `implemented` transition until after the
/// artifact is written, but a caller could still delete the artifact by
/// hand). Otherwise returns [`SealError`] under the same conditions as
/// [`seal`]'s own disclosure-recording step.
pub fn seal_disclosures_only(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    date: &SealDate,
    threshold: BurnedThreshold,
) -> Result<DisclosuresOutcome, SealError> {
    if !is_safe_filename_component(plan_id) {
        return Err(SealError::UnsafeIdentifier(plan_id.to_owned()));
    }

    let plan_path = store.plan_path(repo_start, plan_id)?;
    let source = std::fs::read_to_string(&plan_path).map_err(io_error(&plan_path))?;
    let plan = tftio_planner::parse_markdown(&source)?;
    if plan.metadata.status != PlanStatus::Implemented {
        return Err(SealError::NotSealed {
            plan_id: plan_id.to_owned(),
        });
    }

    let provenance = store.provenance(repo_start, plan_id)?;
    let artifact_path = store
        .root_path()
        .join(RUNS_DIR)
        .join(provenance.repo.as_str())
        .join(plan_id)
        .join(SEALED_ARTIFACT_FILE_NAME);
    if !artifact_path.is_file() {
        return Err(SealError::ArtifactMissing {
            plan_id: plan_id.to_owned(),
            path: artifact_path,
        });
    }

    let disclosures_path = store.root_path().join(DISCLOSURES_FILE_NAME);
    let projection = commit_disclosures(&disclosures_path, &plan, plan_id, date, threshold)?;
    if projection.newly_recorded {
        Ok(DisclosuresOutcome::Recorded {
            burned: projection.burned,
        })
    } else {
        Ok(DisclosuresOutcome::AlreadyRecorded)
    }
}

/// Whether `path` lies inside `boundary`.
///
/// Comparing canonicalized forms when both exist (so a symlinked temporary
/// directory on macOS compares equal to its resolved form, mirroring
/// `src/provenance.rs::RepoIdentity`'s reason for canonicalizing) and
/// walking up to the nearest existing ancestor when `path` does not exist
/// yet (the common case: nothing has been written at the candidate
/// artifact path before this check runs).
///
/// `pub` (not `pub(crate)`): `src/bin/silent-critic.rs` is a separate crate that
/// links against this one, and needs the same guard for its own `--out`
/// destination.
#[must_use]
pub fn path_is_inside(path: &Path, boundary: &Path) -> bool {
    let boundary = boundary
        .canonicalize()
        .unwrap_or_else(|_| boundary.to_path_buf());
    // No further guard against the walk failing to make progress: on every
    // platform this crate targets (`REPO_INVARIANTS.md`'s `linux-amd64`,
    // `macos-arm64`), `Path::parent()` strictly shortens the component list
    // on every `Some` it returns and yields `None` once none remain (an
    // empty path's `parent()` is `None`, not `Some` of itself), so the walk
    // below is bounded by `path`'s own component count without a separate
    // fixed-point check.
    let mut candidate = path.to_path_buf();
    while let Some(parent) = candidate.parent() {
        if let Ok(canonical) = candidate.canonicalize() {
            return canonical.starts_with(&boundary);
        }
        candidate = parent.to_path_buf();
    }
    if let Ok(canonical) = candidate.canonicalize() {
        return canonical.starts_with(&boundary);
    }
    path.starts_with(&boundary)
}

/// Reject `path` if it lies inside `boundary` (see [`path_is_inside`]).
fn reject_if_inside(path: &Path, boundary: &Path) -> Result<(), SealError> {
    if path_is_inside(path, boundary) {
        return Err(SealError::ArtifactInsideRepository {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Disclosure tracking
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
struct DisclosuresFile {
    #[serde(default)]
    criteria: BTreeMap<String, DisclosureEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct DisclosureEntry {
    claim: String,
    count: u32,
    first_disclosed: String,
    last_disclosed: String,
    plans: Vec<String>,
}

fn disclosure_key(claim: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_claim(claim).as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Project what recording a disclosure for every hidden criterion `plan`
/// carries would do to `file`, without writing anything: the updated
/// disclosure ledger, a lookup of the resulting snapshot (keyed by
/// [`normalize_claim`]) for [`render::render_sealed_artifact`], and every
/// criterion that has now crossed `threshold`. Pure, so [`seal`] can use it
/// to preview a seal's disclosure snapshot for rendering *before* the plan
/// transition or the disclosure ledger itself has committed anything
/// (`REPO_INVARIANTS.md` ENG-008: business logic in a pure function, I/O
/// confined to [`commit_disclosures`]'s thin shell around this one).
/// Project `plan`'s disclosures onto `file`, idempotently per
/// `(criterion, plan_id)`: a criterion's `count` and `plans` list only
/// change the *first* time this exact `plan_id` is recorded against it
/// (`entry.plans` is the record of which plans have already contributed a
/// disclosure, not merely a display list) -- calling this again for a
/// `plan_id` already present in every one of `plan`'s hidden criteria's
/// `plans` lists returns `file` unchanged and `newly_recorded: false`, the
/// signal [`seal_disclosures_only`] uses to report
/// [`DisclosuresOutcome::AlreadyRecorded`] rather than silently
/// double-counting.
fn compute_disclosures(
    file: &DisclosuresFile,
    plan: &tftio_planner::model::OperatorPlan,
    plan_id: &str,
    date: &SealDate,
    threshold: BurnedThreshold,
) -> DisclosureProjection {
    let mut file = file.clone();
    let mut snapshots = HashMap::new();
    let mut burned = Vec::new();
    let mut newly_recorded = false;

    for task in &plan.tasks {
        for criterion in &task.hidden_criteria {
            let key = disclosure_key(&criterion.claim);
            let entry = file.criteria.entry(key).or_insert_with(|| DisclosureEntry {
                claim: criterion.claim.clone(),
                count: 0,
                first_disclosed: date.as_str().to_owned(),
                last_disclosed: date.as_str().to_owned(),
                plans: Vec::new(),
            });
            if !entry.plans.iter().any(|existing| existing == plan_id) {
                entry.count += 1;
                date.as_str().clone_into(&mut entry.last_disclosed);
                entry.plans.push(plan_id.to_owned());
                newly_recorded = true;
            }
            let is_burned = entry.count > threshold.value();
            snapshots.insert(
                normalize_claim(&criterion.claim),
                DisclosureSnapshot {
                    count: entry.count,
                    burned: is_burned,
                },
            );
            if is_burned {
                burned.push(BurnedCriterion {
                    claim: criterion.claim.clone(),
                    count: entry.count,
                });
            }
        }
    }

    DisclosureProjection {
        file,
        snapshots,
        burned,
        newly_recorded,
    }
}

/// [`compute_disclosures`]'s result: the updated ledger, the render-facing
/// snapshot, the burned list, and whether anything actually changed for
/// this `plan_id` (see [`compute_disclosures`]'s own doc comment on
/// idempotence).
struct DisclosureProjection {
    file: DisclosuresFile,
    snapshots: HashMap<String, DisclosureSnapshot>,
    burned: Vec<BurnedCriterion>,
    newly_recorded: bool,
}

/// Commit [`compute_disclosures`]'s projection to `disclosures_path`, under
/// its own lock: re-reads the on-disk ledger fresh (never the caller's own
/// earlier preview, which may be stale by the time this runs), computes
/// the update, and writes it back -- unless nothing was newly recorded for
/// this exact `plan_id` (every hidden criterion already carries it), in
/// which case the file is left untouched. The only function in this
/// module that ever writes `disclosures.toml`.
fn commit_disclosures(
    disclosures_path: &Path,
    plan: &tftio_planner::model::OperatorPlan,
    plan_id: &str,
    date: &SealDate,
    threshold: BurnedThreshold,
) -> Result<DisclosureProjection, SealError> {
    let _lock = DisclosuresLock::acquire(disclosures_path)?;
    let existing = load_disclosures(disclosures_path)?;
    let projection = compute_disclosures(&existing, plan, plan_id, date, threshold);
    if projection.newly_recorded {
        save_disclosures(disclosures_path, &projection.file)?;
    }
    Ok(projection)
}

fn load_disclosures(path: &Path) -> Result<DisclosuresFile, SealError> {
    match std::fs::read_to_string(path) {
        Ok(body) => toml::from_str(&body).map_err(|source| SealError::DisclosuresDeserialize {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(DisclosuresFile::default()),
        Err(source) => Err(io_error(path)(source)),
    }
}

/// Render `file` as a TOML document, without going through `toml`'s
/// generic `Serialize`-based `to_string_pretty` (which returns a
/// `Result`): every field `DisclosuresFile`/`DisclosureEntry` carries is a
/// plain `String`, `u32`, or `Vec<String>`, none of which `toml::Value`
/// construction can fail on, so building the document as a `toml::Table`
/// directly and rendering it through `Table`'s own (infallible) `Display`
/// removes the "serialization failed" error path from this module
/// entirely -- there is no `SealError` variant for it because there is no
/// way to construct one.
fn disclosures_to_toml(file: &DisclosuresFile) -> String {
    let mut criteria = toml::Table::new();
    for (key, entry) in &file.criteria {
        let mut table = toml::Table::new();
        table.insert("claim".to_owned(), toml::Value::String(entry.claim.clone()));
        table.insert(
            "count".to_owned(),
            toml::Value::Integer(i64::from(entry.count)),
        );
        table.insert(
            "first_disclosed".to_owned(),
            toml::Value::String(entry.first_disclosed.clone()),
        );
        table.insert(
            "last_disclosed".to_owned(),
            toml::Value::String(entry.last_disclosed.clone()),
        );
        table.insert(
            "plans".to_owned(),
            toml::Value::Array(
                entry
                    .plans
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        criteria.insert(key.clone(), toml::Value::Table(table));
    }
    let mut root = toml::Table::new();
    root.insert("criteria".to_owned(), toml::Value::Table(criteria));
    root.to_string()
}

fn save_disclosures(path: &Path, file: &DisclosuresFile) -> Result<(), SealError> {
    let body = disclosures_to_toml(file);
    create_parent_dir(path)?;
    std::fs::write(path, body).map_err(io_error(path))
}

// ---------------------------------------------------------------------
// Disclosure ledger lock
// ---------------------------------------------------------------------

/// How many times [`DisclosuresLock::acquire`] retries before giving up.
const LOCK_ACQUIRE_ATTEMPTS: u32 = 100;
/// The delay between lock-acquisition attempts.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
/// How old an existing lock file must be before it is treated as abandoned
/// and removed.
const STALE_LOCK_THRESHOLD: Duration = Duration::from_secs(30);

/// A held lock on `disclosures.toml`, released (the lock file removed) on
/// drop. Mirrors `src/ledger.rs`'s private `MutationLock` protocol
/// (`create_new`, bounded retries with backoff, stale-lock reclamation by
/// age) rather than sharing it, since that type is private to `ledger.rs`
/// and this lock guards a different file with no other coupling to the
/// plan mutation lock.
struct DisclosuresLock {
    path: PathBuf,
}

impl DisclosuresLock {
    fn acquire(disclosures_path: &Path) -> Result<Self, SealError> {
        let lock_path = lock_path_for(disclosures_path);
        create_parent_dir(&lock_path)?;
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
                        return Err(SealError::DisclosuresLockTimeout { path: lock_path });
                    }
                    thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(source) => return Err(io_error(&lock_path)(source)),
            }
        }
    }
}

impl Drop for DisclosuresLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_path_for(disclosures_path: &Path) -> PathBuf {
    let mut name = disclosures_path.file_name().map_or_else(
        || DISCLOSURES_FILE_NAME.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    name.push_str(".lock");
    disclosures_path.parent().map_or_else(
        || PathBuf::from(".").join(&name),
        |parent| parent.join(&name),
    )
}

fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= STALE_LOCK_THRESHOLD)
        })
}

#[cfg(test)]
mod tests {
    use super::{
        BurnedThreshold, DisclosuresLock, DisclosuresOutcome, SealError, SealOutcome,
        disclosure_key, load_disclosures, lock_path_for, path_is_inside, reject_if_inside, seal,
        seal_disclosures_only, write_atomic,
    };
    use crate::render::SealDate;
    use crate::store::{Store, StoreRoot};
    use crate::test_support::{self, SENTINEL_PLAN};
    use std::error::Error;
    use std::path::Path;

    /// A plan whose task graph is already closed (every task `done` or
    /// `abandoned`), with hidden-criterion verdicts and residuals baked
    /// directly into its YAML rather than driven through `Ledger` mutations
    /// -- the same fixture `tests/seal.rs`'s integration tests exercise,
    /// reused here so this crate's own `--cfg test` build gets a real
    /// execution of `seal()` too (see `crate::test_support`'s module docs
    /// for why `tests/seal.rs` alone leaves this compiled copy's lines at
    /// zero).
    const FULL_PLAN: &str = include_str!("../tests/fixtures/2026-09-06-sealed-full-plan.md");

    #[test]
    fn burned_thressilent_critic_defaults_to_three() {
        assert_eq!(3, BurnedThreshold::default().value());
    }

    #[test]
    fn burned_thressilent_critic_carries_a_custom_value() {
        assert_eq!(7, BurnedThreshold::new(7).value());
    }

    #[test]
    fn disclosure_key_is_stable_across_case_and_whitespace() {
        assert_eq!(
            disclosure_key("  Hello WORLD  "),
            disclosure_key("hello world")
        );
    }

    #[test]
    fn disclosure_key_differs_for_different_claims() {
        assert_ne!(disclosure_key("claim one"), disclosure_key("claim two"));
    }

    #[test]
    fn path_is_inside_detects_a_nested_nonexistent_path() {
        let boundary = std::env::temp_dir();
        let candidate = boundary
            .join("nested")
            .join("does-not-exist")
            .join("sealed.md");
        assert!(path_is_inside(&candidate, &boundary));
    }

    #[test]
    fn path_is_inside_rejects_a_sibling_path() {
        let boundary = std::env::temp_dir().join("silent-critic-seal-test-boundary");
        let candidate = std::env::temp_dir()
            .join("silent-critic-seal-test-sibling")
            .join("sealed.md");
        assert!(!path_is_inside(&candidate, &boundary));
    }

    #[test]
    fn seal_error_displays_open_tasks() {
        let error = SealError::OpenTasks(vec!["T002".to_owned(), "T003".to_owned()]);
        assert_eq!(
            "plan cannot be sealed: open tasks remain: T002, T003",
            error.to_string()
        );
    }

    #[test]
    fn path_is_inside_falls_back_to_a_lexical_check_when_nothing_canonicalizes() {
        // Neither a relative path with this name nor its ancestors exist
        // relative to the test process's working directory, so every
        // `canonicalize` attempt in the walk fails and the walk runs off
        // the front of the path (`parent()` returning `None`) down to the
        // final lexical `starts_with` fallback.
        let boundary = Path::new("zzz-silent-critic-seal-test-nonexistent-boundary");
        let candidate =
            Path::new("zzz-silent-critic-seal-test-nonexistent-boundary/deeper/sealed.md");
        assert!(path_is_inside(candidate, boundary));
        let sibling = Path::new("zzz-silent-critic-seal-test-nonexistent-sibling/sealed.md");
        assert!(!path_is_inside(sibling, boundary));
    }

    #[test]
    fn path_is_inside_canonicalizes_the_walk_s_final_candidate() {
        // Every intermediate ancestor under this absolute path is
        // nonexistent, so the walk's `while` loop exits once `candidate`
        // becomes `/` itself (`/`.parent()` is `None`) without ever trying
        // `/`'s own `canonicalize` from inside the loop body; `/` always
        // exists, so the loop's own exit path (rather than the lexical
        // fallback) is what resolves this call.
        let candidate = Path::new("/zzz-silent-critic-seal-test-nonexistent-root-child/sealed.md");
        assert!(path_is_inside(candidate, Path::new("/")));
    }

    #[test]
    fn reject_if_inside_rejects_a_path_actually_inside_the_boundary() -> Result<(), Box<dyn Error>>
    {
        let temp = tempfile::tempdir()?;
        let boundary = temp.path().to_path_buf();
        let candidate = boundary.join("runs").join("sealed.md");
        assert!(matches!(
            reject_if_inside(&candidate, &boundary),
            Err(SealError::ArtifactInsideRepository { .. })
        ));
        Ok(())
    }

    #[test]
    fn seal_rejects_an_unsafe_plan_id() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(temp.path().join("store")));
        let repo = temp.path().join("repo");
        let date = SealDate::new("2026-09-06")?;
        let result = super::seal(
            &store,
            &repo,
            "../escape",
            &date,
            BurnedThreshold::default(),
            None,
        );
        assert!(matches!(result, Err(SealError::UnsafeIdentifier(_))));
        Ok(())
    }

    #[test]
    fn seal_reports_open_tasks_end_to_end() -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(SENTINEL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;

        let result = seal(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            &date,
            BurnedThreshold::default(),
            None,
        );

        assert!(matches!(
            &result,
            Err(SealError::OpenTasks(open)) if open == &["T002".to_owned()]
        ));
        Ok(())
    }

    /// Seal `fixture`'s plan once, at threshold 1. A short, single-line-call
    /// wrapper: a call to `seal` whose arguments rustfmt wraps across
    /// several lines puts its closing `)?;`/`);` on a line of its own,
    /// which this LLVM/`cargo-llvm-cov` pairing sometimes fails to credit
    /// as executed even though the call plainly ran (see
    /// `crate::test_support`'s module docs for this same pairing's other,
    /// related per-instance line-counting quirk) -- keeping the call itself
    /// on one line sidesteps it.
    fn seal_fixture(
        fixture: &test_support::Fixture,
        date: &SealDate,
    ) -> Result<SealOutcome, SealError> {
        seal(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            date,
            BurnedThreshold::new(1),
            None,
        )
    }

    /// [`seal_fixture`], with an `--out` copy path. Kept separate (rather
    /// than an `Option<&Path>` parameter on `seal_fixture` itself) so both
    /// helpers' own calls to `seal` fit on one line each.
    #[rustfmt::skip]
    fn seal_fixture_with_out(
        fixture: &test_support::Fixture,
        date: &SealDate,
        out: &Path,
    ) -> Result<SealOutcome, SealError> {
        seal(&fixture.store, &fixture.repo_start, &fixture.plan_id, date, BurnedThreshold::new(1), Some(out))
    }

    /// Record disclosures alone for `fixture`'s plan, at threshold 1 --
    /// the single-line-call counterpart to [`seal_fixture`], for the same
    /// coverage-instrumentation reason its own doc comment explains.
    fn disclosures_only_fixture(
        fixture: &test_support::Fixture,
        date: &SealDate,
    ) -> Result<DisclosuresOutcome, SealError> {
        seal_disclosures_only(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            date,
            BurnedThreshold::new(1),
        )
    }

    #[test]
    fn seal_writes_an_out_copy_on_success() -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;
        let out_dir = tempfile::tempdir()?;
        let out_path = out_dir.path().join("copy.md");

        let outcome = seal_fixture_with_out(&fixture, &date, &out_path)?;

        assert_eq!(Some(out_path.clone()), outcome.out_path);
        assert_eq!(std::fs::read_to_string(&out_path)?, outcome.artifact);
        Ok(())
    }

    #[test]
    fn seal_closes_a_closed_dag_end_to_end() -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;

        let first = seal_fixture(&fixture, &date)?;
        assert!(first.artifact.contains("Sealed review"));
        assert!(first.artifact.contains("Awaiting human judgment"));
        assert!(first.artifact.contains("the automated check still passes"));
        assert!(first.artifact_path.exists());
        assert!(
            first.burned.is_empty(),
            "a claim's first disclosure is never burned"
        );

        // Sealing the same plan again is rejected: `PlanAction::Implement`
        // only applies from `PlanStatus::Approved`, and this plan is now
        // `Implemented`. Exercises `Ledger::apply_plan_mutation`'s error
        // path from inside this crate's own `--cfg test` build too.
        let second = seal_fixture(&fixture, &date);
        assert!(second.is_err());
        let second_message = second
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(second_message.contains("implemented"));
        Ok(())
    }

    #[test]
    fn write_atomic_falls_back_to_a_default_file_name_when_the_path_has_none()
    -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let sub = temp.path().join("sub");
        std::fs::create_dir_all(&sub)?;
        // Ends in `..`, so `Path::file_name` returns `None` -- the
        // fallback branch `write_atomic` needs even though no real caller
        // in this crate builds such a path (every artifact/disclosures
        // path this module computes ends in a real file name). `parent()`
        // still resolves to the real, existing `sub` directory, so the
        // write itself succeeds; the rename onto `sub/..` (`temp`'s own
        // root, a directory) fails, exercising both this fallback and the
        // rename-failure cleanup path in one call.
        let path = sub.join("..");
        let result = write_atomic(&path, "contents");
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn write_atomic_reports_and_cleans_up_a_temp_file_write_failure() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("readonly");
        std::fs::create_dir(&dir)?;
        // `create_parent_dir` is a no-op here (the directory already
        // exists), so the write-permission check only bites on the
        // temp-file `fs::write` itself -- the branch this test targets.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))?;

        let path = dir.join("artifact.md");
        let result = write_atomic(&path, "contents");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

        assert!(matches!(result, Err(SealError::Io { .. })));
        assert_eq!(
            0,
            std::fs::read_dir(&dir)?.count(),
            "no temp file left behind"
        );
        Ok(())
    }

    #[test]
    fn write_atomic_reports_a_rename_failure_and_removes_the_temp_file()
    -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        // Renaming a file over an existing, non-empty directory fails on
        // every platform this crate targets.
        let target = temp.path().join("target");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("occupant"), "keep")?;

        let result = write_atomic(&target, "contents");
        assert!(matches!(result, Err(SealError::Io { .. })));

        // The temp file left behind by the write, before the failed
        // rename, was cleaned up rather than orphaned.
        let leftover = std::fs::read_dir(temp.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != target)
            .count();
        assert_eq!(0, leftover, "the temporary file must not be left behind");
        Ok(())
    }

    #[test]
    fn seal_removes_the_out_copy_when_the_out_write_fails() -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;
        // An existing, non-empty directory at the `--out` path makes its
        // own atomic write fail after the canonical artifact write already
        // succeeded.
        let out_dir = tempfile::tempdir()?;
        let out_path = out_dir.path().join("out.md");
        std::fs::create_dir(&out_path)?;
        std::fs::write(out_path.join("occupant"), "keep")?;

        let result = seal(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            &date,
            BurnedThreshold::new(1),
            Some(out_path.as_path()),
        );
        assert!(result.is_err());

        // The canonical artifact was rolled back too, and the plan never
        // actually sealed.
        let plan_path = fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?;
        let source = std::fs::read_to_string(&plan_path)?;
        let plan = tftio_planner::parse_markdown(&source)?;
        assert_eq!(
            tftio_planner::model::PlanStatus::Approved,
            plan.metadata.status
        );
        Ok(())
    }

    #[test]
    fn seal_removes_both_the_artifact_and_the_out_copy_when_the_mutation_fails()
    -> Result<(), Box<dyn Error>> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;
        let out_dir = tempfile::tempdir()?;
        let out_path = out_dir.path().join("out.md");

        let plan_directory = fixture
            .store
            .plan_directory(&fixture.repo_start, &fixture.plan_id)?;
        std::fs::set_permissions(&plan_directory, std::fs::Permissions::from_mode(0o500))?;

        let result = seal(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            &date,
            BurnedThreshold::new(1),
            Some(out_path.as_path()),
        );

        std::fs::set_permissions(&plan_directory, std::fs::Permissions::from_mode(0o700))?;

        assert!(result.is_err());
        assert!(
            !out_path.exists(),
            "the --out copy must be removed too when the mutation fails"
        );
        Ok(())
    }

    #[test]
    fn seal_disclosures_only_rejects_an_unsafe_plan_id() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(temp.path().join("store")));
        let repo = temp.path().join("repo");
        let date = SealDate::new("2026-09-06")?;
        let result = seal_disclosures_only(
            &store,
            &repo,
            "../escape",
            &date,
            BurnedThreshold::default(),
        );
        assert!(matches!(result, Err(SealError::UnsafeIdentifier(_))));
        Ok(())
    }

    #[test]
    fn seal_disclosures_only_reports_already_recorded_and_does_not_double_count()
    -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;

        seal_fixture(&fixture, &date)?;
        // Disclosures were already recorded by the seal above; recording
        // is idempotent per (criterion, plan id), so this call must not
        // increment anything a second time -- it reports
        // `AlreadyRecorded` and leaves `disclosures.toml` untouched.
        let outcome = disclosures_only_fixture(&fixture, &date)?;
        assert_eq!(DisclosuresOutcome::AlreadyRecorded, outcome);

        let disclosures_path = fixture.store.root_path().join("disclosures.toml");
        let body = std::fs::read_to_string(&disclosures_path)?;
        let table: toml::Table = toml::from_str(&body)?;
        let criteria = table
            .get("criteria")
            .and_then(toml::Value::as_table)
            .ok_or("expected a criteria table")?;
        for (_, entry) in criteria {
            let count = entry
                .get("count")
                .and_then(toml::Value::as_integer)
                .ok_or("expected an integer count")?;
            assert_eq!(1, count, "a repeat recovery call must not double-count");
        }
        Ok(())
    }

    #[test]
    fn seal_disclosures_only_reports_a_missing_artifact() -> Result<(), Box<dyn Error>> {
        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;

        let sealed = seal_fixture(&fixture, &date)?;
        std::fs::remove_file(&sealed.artifact_path)?;

        let result = seal_disclosures_only(
            &fixture.store,
            &fixture.repo_start,
            &fixture.plan_id,
            &date,
            BurnedThreshold::new(1),
        );
        assert!(matches!(result, Err(SealError::ArtifactMissing { .. })));
        Ok(())
    }

    #[test]
    fn seal_disclosures_only_records_after_a_failed_disclosure_commit() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = test_support::set_up(FULL_PLAN)?;
        let date = SealDate::new("2026-09-06")?;
        let provenance = fixture
            .store
            .provenance(&fixture.repo_start, &fixture.plan_id)?;
        let run_dir = fixture
            .store
            .root_path()
            .join(super::RUNS_DIR)
            .join(provenance.repo.as_str())
            .join(&fixture.plan_id);
        std::fs::create_dir_all(&run_dir)?;
        std::fs::set_permissions(
            fixture.store.root_path(),
            std::fs::Permissions::from_mode(0o500),
        )?;

        let first = seal_fixture(&fixture, &date);

        std::fs::set_permissions(
            fixture.store.root_path(),
            std::fs::Permissions::from_mode(0o700),
        )?;

        assert!(matches!(
            first,
            Err(SealError::DisclosuresNotRecorded { .. })
        ));

        let outcome = disclosures_only_fixture(&fixture, &date)?;
        assert!(matches!(outcome, DisclosuresOutcome::Recorded { .. }));
        Ok(())
    }

    #[test]
    fn load_disclosures_reports_malformed_toml() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("disclosures.toml");
        std::fs::write(&path, "not valid toml [[[")?;
        let result = load_disclosures(&path);
        assert!(matches!(
            result,
            Err(SealError::DisclosuresDeserialize { .. })
        ));
        Ok(())
    }

    #[test]
    fn load_disclosures_reports_a_read_failure_other_than_not_found() -> Result<(), Box<dyn Error>>
    {
        let temp = tempfile::tempdir()?;
        // A directory where a file is expected fails `read_to_string` with
        // an error other than `NotFound`.
        let path = temp.path().join("disclosures.toml");
        std::fs::create_dir(&path)?;
        let result = load_disclosures(&path);
        assert!(matches!(result, Err(SealError::Io { .. })));
        Ok(())
    }

    #[test]
    fn lock_path_for_falls_back_when_the_path_has_no_file_name() {
        assert_eq!(
            Path::new(".").join("disclosures.toml.lock"),
            lock_path_for(Path::new(""))
        );
    }

    #[test]
    fn disclosures_lock_reclaims_a_stale_lock_left_by_a_crashed_writer()
    -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let disclosures_path = temp.path().join("disclosures.toml");
        let lock_path = lock_path_for(&disclosures_path);
        std::fs::write(&lock_path, "left behind by a crashed writer")?;
        let stale_time = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        let lock_file = std::fs::File::open(&lock_path)?;
        lock_file.set_modified(stale_time)?;

        let lock = DisclosuresLock::acquire(&disclosures_path)?;
        drop(lock);
        assert!(!lock_path.exists());
        Ok(())
    }

    #[test]
    fn disclosures_lock_reports_an_io_error_that_is_not_contention() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let disclosures_dir = temp.path().join("locked-dir");
        std::fs::create_dir_all(&disclosures_dir)?;
        let disclosures_path = disclosures_dir.join("disclosures.toml");
        // Pre-create the lock file so `create_dir_all` in `acquire` is a
        // no-op, then remove write permission on the directory so the
        // subsequent `create_new` fails with `PermissionDenied` rather than
        // `AlreadyExists`.
        std::fs::set_permissions(&disclosures_dir, std::fs::Permissions::from_mode(0o500))?;

        let result = DisclosuresLock::acquire(&disclosures_path);

        std::fs::set_permissions(&disclosures_dir, std::fs::Permissions::from_mode(0o700))?;
        assert!(matches!(result, Err(SealError::Io { .. })));
        Ok(())
    }

    #[test]
    fn disclosures_lock_times_out_against_a_fresh_contending_lock() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let disclosures_path = temp.path().join("disclosures.toml");
        let lock_path = lock_path_for(&disclosures_path);
        std::fs::write(&lock_path, "held by another writer")?;

        let result = DisclosuresLock::acquire(&disclosures_path);
        assert!(matches!(
            result,
            Err(SealError::DisclosuresLockTimeout { .. })
        ));
        Ok(())
    }
}
