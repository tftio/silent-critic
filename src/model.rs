//! The supervision domain model.
//!
//! Pure, I/O-free types the rest of the system moves around: run and task
//! execution state, evidence with a coarse provenance stamp, the
//! per-criterion verdict (including the structurally-non-empty
//! `evidence_needed` an `undetermined` judgment must carry), and the
//! residual types that carry unresolved items to the operator.
//!
//! This module has no dependency on the filesystem, environment, clock, or
//! subprocess corners of `std` (see the acceptance test at the bottom of this
//! file), and does not model mediation, an independence lattice, or
//! certainty scores — those belong to later work, not here.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------

/// The identifier of a plan document, as assigned by `tftio_planner`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(String);

impl PlanId {
    /// Wrap a raw plan identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identifier of a task within a plan's task graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    /// Wrap a raw task identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identifier of an acceptance criterion declared on a task.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CriterionId(String);

impl CriterionId {
    /// Wrap a raw criterion identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CriterionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identifier of a piece of evidence gathered for a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceId(String);

impl EvidenceId {
    /// Wrap a raw evidence identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvidenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity of a repository under supervision, as a boundary-facing
/// newtype rather than a filesystem path.
///
/// This carries whatever a worktree resolves to (a URL, a local checkout
/// name, an on-disk location) as an opaque string; converting an actual
/// filesystem path into one is the job of the boundary code that has a
/// `std::path::PathBuf` in hand, not of this module.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepositoryLocator(String);

impl RepositoryLocator {
    /// Wrap a raw repository locator.
    #[must_use]
    pub fn new(locator: impl Into<String>) -> Self {
        Self(locator.into())
    }

    /// Borrow the raw locator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryLocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A base ref (branch, tag, or commit-ish) a run is anchored to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseRef(String);

impl BaseRef {
    /// Wrap a raw base ref.
    #[must_use]
    pub fn new(base_ref: impl Into<String>) -> Self {
        Self(base_ref.into())
    }

    /// Borrow the raw ref.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BaseRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A repository-relative path, carried as an opaque string rather than
/// `std::path::PathBuf` so this module stays filesystem-free.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChangedPath(String);

impl ChangedPath {
    /// Wrap a raw repository-relative path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrow the raw path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChangedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------
// Non-empty string
// ---------------------------------------------------------------------

/// A string guaranteed non-empty after trimming Unicode whitespace.
///
/// This is the general building block behind [`EvidenceNeeded`]: any place
/// the model wants "some text, but not nothing" reaches for this instead of
/// a bare `String` plus a runtime check.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NonEmptyString(String);

/// The reason a [`NonEmptyString`] could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyStringError;

impl fmt::Display for EmptyStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value must not be empty or all whitespace")
    }
}

impl std::error::Error for EmptyStringError {}

impl NonEmptyString {
    /// Build a `NonEmptyString`, rejecting empty or all-whitespace input.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyStringError`] when `value` is empty after trimming
    /// Unicode whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptyStringError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EmptyStringError);
        }
        Ok(Self(value))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NonEmptyString {
    type Error = EmptyStringError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<NonEmptyString> for String {
    fn from(value: NonEmptyString) -> Self {
        value.0
    }
}

/// What would resolve an [`Judgment::Undetermined`] judgment.
///
/// Structurally non-empty: there is no way to name "the evidence needed" as
/// blank text, at construction or at deserialization. Serializes and
/// deserializes as a plain string (the inner [`NonEmptyString`] does that
/// transparently), with deserialization routed through [`TryFrom<String>`]
/// so the same emptiness check applies on the way in from the judge channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct EvidenceNeeded(NonEmptyString);

impl EvidenceNeeded {
    /// Name the evidence that would resolve the judgment, rejecting empty or
    /// all-whitespace input.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyStringError`] when `description` is empty after
    /// trimming Unicode whitespace.
    pub fn new(description: impl Into<String>) -> Result<Self, EmptyStringError> {
        Ok(Self(NonEmptyString::new(description)?))
    }

    /// Borrow the description of the evidence needed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<String> for EvidenceNeeded {
    type Error = EmptyStringError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

// ---------------------------------------------------------------------
// Run and task execution state
// ---------------------------------------------------------------------

/// A supervised run: a plan, the repository it operates on, and the base ref
/// it is anchored to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    plan_id: PlanId,
    repository: RepositoryLocator,
    base_ref: BaseRef,
}

impl Run {
    /// Build a run over the given plan, repository, and base ref.
    #[must_use]
    pub const fn new(plan_id: PlanId, repository: RepositoryLocator, base_ref: BaseRef) -> Self {
        Self {
            plan_id,
            repository,
            base_ref,
        }
    }

    /// The plan this run executes.
    #[must_use]
    pub const fn plan_id(&self) -> &PlanId {
        &self.plan_id
    }

    /// The repository this run operates on.
    #[must_use]
    pub const fn repository(&self) -> &RepositoryLocator {
        &self.repository
    }

    /// The base ref this run is anchored to.
    #[must_use]
    pub const fn base_ref(&self) -> &BaseRef {
        &self.base_ref
    }
}

/// The execution state of a single task within a run.
///
/// A closed set: pending (not yet dispatched), dispatched (a worker is
/// working it), submitted (the worker has reported completion), judged (a
/// verdict has been rendered), sealed (the task's disposition is final and
/// no further judging will occur).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionState {
    /// The task has not yet been dispatched to a worker.
    Pending,
    /// The task has been dispatched to a worker.
    Dispatched,
    /// The worker has submitted the task as complete.
    Submitted,
    /// A verdict has been rendered for the task.
    Judged,
    /// The task's disposition is final.
    Sealed,
}

// ---------------------------------------------------------------------
// Evidence and provenance
// ---------------------------------------------------------------------

/// The coarse provenance of a piece of evidence: who or what produced it.
///
/// A closed enum with exactly four variants, dispatched exhaustively
/// wherever provenance matters, with no catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProvenance {
    /// Produced by an automated tool (a check, a linter, a test runner).
    ToolAuthored,
    /// Produced by the judge's own reasoning.
    Judge,
    /// Reported by the worker in its own narration, unverified by a tool.
    WorkerNarrated,
    /// Observed directly by the human operator.
    OperatorObserved,
}

/// A single piece of evidence gathered in service of a criterion judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    id: EvidenceId,
    provenance: EvidenceProvenance,
    summary: NonEmptyString,
}

impl Evidence {
    /// Build a piece of evidence.
    #[must_use]
    pub const fn new(
        id: EvidenceId,
        provenance: EvidenceProvenance,
        summary: NonEmptyString,
    ) -> Self {
        Self {
            id,
            provenance,
            summary,
        }
    }

    /// This evidence's identifier.
    #[must_use]
    pub const fn id(&self) -> &EvidenceId {
        &self.id
    }

    /// This evidence's provenance.
    #[must_use]
    pub const fn provenance(&self) -> EvidenceProvenance {
        self.provenance
    }

    /// A human-readable summary of what this evidence shows.
    #[must_use]
    pub fn summary(&self) -> &str {
        self.summary.as_str()
    }
}

// ---------------------------------------------------------------------
// Criteria
// ---------------------------------------------------------------------

/// How a criterion is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorKind {
    /// Evaluated by an automated check (a test, a lint, a script).
    Automated,
    /// Evaluated by an agent (the judge) reasoning over evidence.
    AgentEvaluated,
    /// Evaluated only by human judgment.
    HumanJudgment,
}

/// Whether a criterion's existence is visible to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionVisibility {
    /// The worker can see this criterion.
    Visible,
    /// The worker cannot see this criterion; it is judge- and
    /// operator-facing only.
    Hidden,
}

/// An acceptance criterion attached to a task, carrying enough to attribute
/// later findings: how it is evaluated and whether the worker can see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    id: CriterionId,
    evaluator: EvaluatorKind,
    visibility: CriterionVisibility,
}

impl Criterion {
    /// Build a criterion.
    #[must_use]
    pub const fn new(
        id: CriterionId,
        evaluator: EvaluatorKind,
        visibility: CriterionVisibility,
    ) -> Self {
        Self {
            id,
            evaluator,
            visibility,
        }
    }

    /// This criterion's identifier.
    #[must_use]
    pub const fn id(&self) -> &CriterionId {
        &self.id
    }

    /// How this criterion is evaluated.
    #[must_use]
    pub const fn evaluator(&self) -> EvaluatorKind {
        self.evaluator
    }

    /// Whether this criterion is visible to the worker.
    #[must_use]
    pub const fn visibility(&self) -> CriterionVisibility {
        self.visibility
    }
}

// ---------------------------------------------------------------------
// Judgment and verdict
// ---------------------------------------------------------------------

/// The per-criterion judgment: pass, fail, or undetermined.
///
/// `Undetermined` structurally requires a non-empty [`EvidenceNeeded`]: there
/// is no constructor path, and no deserialization path, that produces an
/// `Undetermined` naming empty evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Judgment {
    /// The criterion was met.
    Pass,
    /// The criterion was not met.
    Fail,
    /// The available evidence could not settle the criterion.
    Undetermined {
        /// What would resolve the judgment.
        evidence_needed: EvidenceNeeded,
    },
}

/// A rendered verdict for one criterion: the judgment plus the reasoning
/// behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    criterion_id: CriterionId,
    judgment: Judgment,
    rationale: NonEmptyString,
}

impl Verdict {
    /// Build a verdict for a criterion.
    #[must_use]
    pub const fn new(
        criterion_id: CriterionId,
        judgment: Judgment,
        rationale: NonEmptyString,
    ) -> Self {
        Self {
            criterion_id,
            judgment,
            rationale,
        }
    }

    /// The criterion this verdict judges.
    #[must_use]
    pub const fn criterion_id(&self) -> &CriterionId {
        &self.criterion_id
    }

    /// The judgment rendered.
    #[must_use]
    pub const fn judgment(&self) -> &Judgment {
        &self.judgment
    }

    /// The rationale behind the judgment.
    #[must_use]
    pub fn rationale(&self) -> &str {
        self.rationale.as_str()
    }
}

/// The run-level disposition: what the operator should do with a run's
/// verdicts taken together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// The run is accepted as complete.
    Accept,
    /// The run is rejected.
    Reject,
    /// The run requires an operator decision before it can be resolved.
    NeedsOperator,
}

// ---------------------------------------------------------------------
// Residuals
// ---------------------------------------------------------------------

/// An unresolved item that must be carried to the operator.
///
/// A closed enum: a human-judgment criterion still awaiting the operator, an
/// undetermined judgment naming its evidence needed, changed scope a task's
/// declared `files.likely_modify` did not cover, and disagreement between two
/// verdicts for the same criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residual {
    /// A criterion evaluated only by human judgment, awaiting the operator.
    AwaitingHumanJudgment {
        /// The criterion awaiting the operator's judgment.
        criterion_id: CriterionId,
    },
    /// A criterion judgment that came back undetermined.
    UndeterminedJudgment {
        /// The criterion that could not be settled.
        criterion_id: CriterionId,
        /// What would resolve the judgment.
        evidence_needed: EvidenceNeeded,
    },
    /// A task changed paths its declared `files.likely_modify` did not
    /// cover.
    UncoveredChangedScope {
        /// The task whose changes exceeded its declared scope.
        task_id: TaskId,
        /// The changed paths not covered by the declared scope.
        paths: Vec<ChangedPath>,
    },
    /// Two verdicts were rendered for the same criterion and disagree.
    JudgeDisagreement {
        /// The criterion two verdicts disagree on.
        criterion_id: CriterionId,
        /// The first verdict.
        first: Box<Verdict>,
        /// The second verdict.
        second: Box<Verdict>,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        BaseRef, ChangedPath, Criterion, CriterionId, CriterionVisibility, Disposition,
        EmptyStringError, EvaluatorKind, Evidence, EvidenceId, EvidenceNeeded, EvidenceProvenance,
        Judgment, NonEmptyString, PlanId, RepositoryLocator, Residual, Run, TaskExecutionState,
        TaskId, Verdict,
    };
    use std::error::Error;

    #[test]
    fn non_empty_string_rejects_empty_and_whitespace() {
        assert_eq!(Err(EmptyStringError), NonEmptyString::new(""));
        assert_eq!(Err(EmptyStringError), NonEmptyString::new("   \t\n"));
    }

    #[test]
    fn non_empty_string_accepts_content() -> Result<(), Box<dyn Error>> {
        let value = NonEmptyString::new("hello")?;
        assert_eq!("hello", value.as_str());
        Ok(())
    }

    #[test]
    fn evidence_needed_accepts_content() -> Result<(), Box<dyn Error>> {
        let value = EvidenceNeeded::new("a passing CI run")?;
        assert_eq!("a passing CI run", value.as_str());
        Ok(())
    }

    #[test]
    fn evidence_needed_rejects_empty_at_construction() {
        assert_eq!(Err(EmptyStringError), EvidenceNeeded::new(""));
        assert_eq!(Err(EmptyStringError), EvidenceNeeded::new("   "));
    }

    #[test]
    fn evidence_needed_rejects_empty_at_deserialization() {
        let empty_evidence_needed = r#"{"undetermined":{"evidence_needed":""}}"#;
        let result: Result<Judgment, _> = serde_json::from_str(empty_evidence_needed);
        assert!(
            result.is_err(),
            "expected empty evidence_needed to be rejected"
        );

        let whitespace_evidence_needed = r#"{"undetermined":{"evidence_needed":"   "}}"#;
        let result: Result<Judgment, _> = serde_json::from_str(whitespace_evidence_needed);
        assert!(
            result.is_err(),
            "expected whitespace evidence_needed to be rejected"
        );
    }

    #[test]
    fn evidence_needed_accepts_non_empty_at_deserialization() -> Result<(), Box<dyn Error>> {
        let json = r#"{"undetermined":{"evidence_needed":"a failing test run"}}"#;
        let judgment: Judgment = serde_json::from_str(json)?;
        assert_eq!(
            Judgment::Undetermined {
                evidence_needed: EvidenceNeeded::new("a failing test run")?
            },
            judgment
        );
        Ok(())
    }

    fn sample_verdict(criterion: &str, judgment: Judgment) -> Result<Verdict, Box<dyn Error>> {
        Ok(Verdict::new(
            CriterionId::new(criterion),
            judgment,
            NonEmptyString::new("rationale text")?,
        ))
    }

    #[test]
    fn every_judgment_round_trips_through_judge_channel_json() -> Result<(), Box<dyn Error>> {
        let judgments = [
            Judgment::Pass,
            Judgment::Fail,
            Judgment::Undetermined {
                evidence_needed: EvidenceNeeded::new("a reproduction log")?,
            },
        ];

        for judgment in judgments {
            let verdict = sample_verdict("crit-1", judgment)?;
            let serialized = serde_json::to_string(&verdict)?;
            let round_tripped: Verdict = serde_json::from_str(&serialized)?;
            assert_eq!(verdict, round_tripped);
        }
        Ok(())
    }

    #[test]
    fn every_disposition_round_trips() -> Result<(), Box<dyn Error>> {
        let dispositions = [
            Disposition::Accept,
            Disposition::Reject,
            Disposition::NeedsOperator,
        ];
        for disposition in dispositions {
            let serialized = serde_json::to_string(&disposition)?;
            let round_tripped: Disposition = serde_json::from_str(&serialized)?;
            assert_eq!(disposition, round_tripped);
        }
        Ok(())
    }

    #[test]
    fn every_evidence_provenance_round_trips() -> Result<(), Box<dyn Error>> {
        let provenances = [
            EvidenceProvenance::ToolAuthored,
            EvidenceProvenance::Judge,
            EvidenceProvenance::WorkerNarrated,
            EvidenceProvenance::OperatorObserved,
        ];
        for provenance in provenances {
            let evidence = Evidence::new(
                EvidenceId::new("ev-1"),
                provenance,
                NonEmptyString::new("summary")?,
            );
            let serialized = serde_json::to_string(&evidence)?;
            let round_tripped: Evidence = serde_json::from_str(&serialized)?;
            assert_eq!(evidence, round_tripped);
        }
        Ok(())
    }

    #[test]
    fn every_task_execution_state_round_trips() -> Result<(), Box<dyn Error>> {
        let states = [
            TaskExecutionState::Pending,
            TaskExecutionState::Dispatched,
            TaskExecutionState::Submitted,
            TaskExecutionState::Judged,
            TaskExecutionState::Sealed,
        ];
        for state in states {
            let serialized = serde_json::to_string(&state)?;
            let round_tripped: TaskExecutionState = serde_json::from_str(&serialized)?;
            assert_eq!(state, round_tripped);
        }
        Ok(())
    }

    #[test]
    fn every_evaluator_kind_and_visibility_round_trips() -> Result<(), Box<dyn Error>> {
        let evaluators = [
            EvaluatorKind::Automated,
            EvaluatorKind::AgentEvaluated,
            EvaluatorKind::HumanJudgment,
        ];
        let visibilities = [CriterionVisibility::Visible, CriterionVisibility::Hidden];
        for evaluator in evaluators {
            for visibility in visibilities {
                let criterion = Criterion::new(CriterionId::new("crit-1"), evaluator, visibility);
                let serialized = serde_json::to_string(&criterion)?;
                let round_tripped: Criterion = serde_json::from_str(&serialized)?;
                assert_eq!(criterion, round_tripped);
            }
        }
        Ok(())
    }

    #[test]
    fn every_residual_variant_round_trips_through_judge_channel_json() -> Result<(), Box<dyn Error>>
    {
        let residuals = [
            Residual::AwaitingHumanJudgment {
                criterion_id: CriterionId::new("crit-human"),
            },
            Residual::UndeterminedJudgment {
                criterion_id: CriterionId::new("crit-undetermined"),
                evidence_needed: EvidenceNeeded::new("a manual repro")?,
            },
            Residual::UncoveredChangedScope {
                task_id: TaskId::new("task-1"),
                paths: vec![ChangedPath::new("src/unexpected.rs")],
            },
            Residual::JudgeDisagreement {
                criterion_id: CriterionId::new("crit-disagreement"),
                first: Box::new(sample_verdict("crit-disagreement", Judgment::Pass)?),
                second: Box::new(sample_verdict("crit-disagreement", Judgment::Fail)?),
            },
        ];

        for residual in residuals {
            let serialized = serde_json::to_string(&residual)?;
            let round_tripped: Residual = serde_json::from_str(&serialized)?;
            assert_eq!(residual, round_tripped);
        }
        Ok(())
    }

    #[test]
    fn run_exposes_its_fields() {
        let run = Run::new(
            PlanId::new("plan-1"),
            RepositoryLocator::new("git@example.com:org/repo.git"),
            BaseRef::new("main"),
        );
        assert_eq!("plan-1", run.plan_id().as_str());
        assert_eq!("git@example.com:org/repo.git", run.repository().as_str());
        assert_eq!("main", run.base_ref().as_str());
    }

    #[test]
    fn evidence_exposes_its_fields() -> Result<(), Box<dyn Error>> {
        let evidence = Evidence::new(
            EvidenceId::new("ev-1"),
            EvidenceProvenance::ToolAuthored,
            NonEmptyString::new("a green test run")?,
        );
        assert_eq!("ev-1", evidence.id().as_str());
        assert_eq!(EvidenceProvenance::ToolAuthored, evidence.provenance());
        assert_eq!("a green test run", evidence.summary());
        Ok(())
    }

    #[test]
    fn criterion_exposes_its_fields() {
        let criterion = Criterion::new(
            CriterionId::new("crit-1"),
            EvaluatorKind::Automated,
            CriterionVisibility::Hidden,
        );
        assert_eq!("crit-1", criterion.id().as_str());
        assert_eq!(EvaluatorKind::Automated, criterion.evaluator());
        assert_eq!(CriterionVisibility::Hidden, criterion.visibility());
    }

    #[test]
    fn verdict_exposes_its_fields() -> Result<(), Box<dyn Error>> {
        let verdict = sample_verdict("crit-1", Judgment::Pass)?;
        assert_eq!("crit-1", verdict.criterion_id().as_str());
        assert_eq!(&Judgment::Pass, verdict.judgment());
        assert_eq!("rationale text", verdict.rationale());
        Ok(())
    }

    #[test]
    fn ids_display_their_raw_value() {
        assert_eq!("plan-1", PlanId::new("plan-1").to_string());
        assert_eq!("task-1", TaskId::new("task-1").to_string());
        assert_eq!("crit-1", CriterionId::new("crit-1").to_string());
        assert_eq!("ev-1", EvidenceId::new("ev-1").to_string());
        assert_eq!(
            "git@example.com:org/repo.git",
            RepositoryLocator::new("git@example.com:org/repo.git").to_string()
        );
        assert_eq!("main", BaseRef::new("main").to_string());
        assert_eq!("src/foo.rs", ChangedPath::new("src/foo.rs").to_string());
    }

    #[test]
    fn ids_expose_their_raw_value_via_as_str() {
        assert_eq!("plan-1", PlanId::new("plan-1").as_str());
        assert_eq!("task-1", TaskId::new("task-1").as_str());
        assert_eq!("crit-1", CriterionId::new("crit-1").as_str());
        assert_eq!("ev-1", EvidenceId::new("ev-1").as_str());
        assert_eq!(
            "git@example.com:org/repo.git",
            RepositoryLocator::new("git@example.com:org/repo.git").as_str()
        );
        assert_eq!("main", BaseRef::new("main").as_str());
        assert_eq!("src/foo.rs", ChangedPath::new("src/foo.rs").as_str());
    }

    #[test]
    fn empty_string_error_displays_a_message() {
        assert_eq!(
            "value must not be empty or all whitespace",
            EmptyStringError.to_string()
        );
    }

    #[test]
    fn module_avoids_io_clock_env_and_process_dependencies() {
        // Each banned substring is assembled at runtime from fragments so this
        // very check does not itself contain the substring it is scanning for.
        let source = include_str!("model.rs");
        let banned = [
            format!("{}{}", "std:", ":fs"),
            format!("{}{}", "std:", ":env"),
            format!("{}{}", "std:", ":time"),
            format!("{}{}", "std:", ":process"),
        ];
        for pattern in &banned {
            assert!(
                !source.contains(pattern.as_str()),
                "src/model.rs must not reference {pattern}"
            );
        }
    }
}
