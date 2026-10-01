//! Rendering the sealed review artifact: Markdown suitable for a merge
//! request comment.
//!
//! Pure and deterministic: every function here takes the plan, the
//! unresolved items, and the disclosure snapshots as data and returns a
//! `String`; none of it touches the filesystem or the clock (`src/seal.rs`
//! is the imperative shell that reads the plan, tracks disclosures, and
//! writes the result). The seal date is a typed [`SealDate`] the caller
//! supplies at the process edge, not a clock read from inside this module.
//!
//! # Ordering
//!
//! The rendering is residual-first, so what needs the operator's attention
//! comes before what passed: a one-line header, then every unresolved item (undetermined judgments,
//! human-judgment criteria awaiting the operator, uncovered changed scope,
//! judge disagreement, abandoned tasks) grouped by task, then the disclosed
//! hidden criteria per task, then the visible criteria and automated checks
//! per task with routine passing checks last, then the Decision Log and
//! Operator Guidance Log verbatim, then a footer. [`render_sealed_artifact`]
//! is the only place this order is assembled; nothing else in this crate
//! reconstructs it.
//!
//! # Parsing `completion_evidence`
//!
//! `src/ledger.rs`'s `render_visible_verdict` and `src/tools.rs`'s
//! `summarize_judge_run` are the only two writers of structured
//! `completion_evidence` text; [`parse_completion_evidence`] and
//! [`classify_completion_evidence_entry`] are this module's read side of
//! that same, otherwise-undocumented convention -- kept here rather than in
//! `src/ledger.rs` because rendering is this module's only consumer.

use std::collections::HashMap;
use std::fmt::Write as _;

use tftio_planner::model::{
    Criticality, Evaluator, HiddenCriterion, HiddenEvidenceRecord, HiddenVerdictJudgment,
    OperatorPlan, OperatorTask, TaskStatus,
};

use crate::ledger::{CriterionAddress, OperatorItem, criterion_address};
use crate::model::{Judgment, Residual};

// ---------------------------------------------------------------------
// Seal date
// ---------------------------------------------------------------------

/// A `YYYY-MM-DD` date supplied by the caller (the CLI, at the process
/// edge) rather than read from a clock inside this module or [`crate::seal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealDate(String);

/// A [`SealDate`] was not shaped `YYYY-MM-DD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealDateError;

impl std::fmt::Display for SealDateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("seal date must be shaped YYYY-MM-DD")
    }
}

impl std::error::Error for SealDateError {}

impl SealDate {
    /// Build a seal date, rejecting anything not shaped `YYYY-MM-DD`.
    ///
    /// # Errors
    ///
    /// Returns [`SealDateError`] when `value` is not four ASCII digits, a
    /// `-`, two ASCII digits, a `-`, and two ASCII digits.
    pub fn new(value: impl Into<String>) -> Result<Self, SealDateError> {
        let value = value.into();
        if date_shaped(&value) {
            Ok(Self(value))
        } else {
            Err(SealDateError)
        }
    }

    /// Borrow the date string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn date_shaped(value: &str) -> bool {
    let mut parts = value.split('-');
    let year = parts.next();
    let month = parts.next();
    let day = parts.next();
    parts.next().is_none()
        && year
            .is_some_and(|part| part.len() == 4 && part.bytes().all(|byte| byte.is_ascii_digit()))
        && month
            .is_some_and(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
        && day.is_some_and(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
}

// ---------------------------------------------------------------------
// Disclosure snapshots
// ---------------------------------------------------------------------

/// How many times a hidden criterion's claim has been disclosed across
/// every sealed run, and whether that count has crossed the configured
/// [`crate::seal::BurnedThreshold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisclosureSnapshot {
    /// Total disclosures recorded for this claim, including this seal.
    pub count: u32,
    /// Whether `count` exceeds the configured burned threshold.
    pub burned: bool,
}

/// Normalize a hidden criterion's claim text into the shared lookup key.
///
/// Trimmed and lowercased, so two claims differing only in surrounding
/// whitespace or case are tracked (by [`crate::seal`]'s disclosure ledger)
/// and looked up (here) as the same disclosure.
#[must_use]
pub fn normalize_claim(claim: &str) -> String {
    claim.trim().to_lowercase()
}

// ---------------------------------------------------------------------
// Rendering input
// ---------------------------------------------------------------------

/// Everything [`render_sealed_artifact`] needs, gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    /// The sealed plan (already carrying `status: implemented` and
    /// `execution.task_graph_status: complete`).
    pub plan: &'a OperatorPlan,
    /// Every unresolved item the plan still carries, from
    /// [`crate::ledger::Ledger::unresolved_items`].
    pub unresolved: &'a [OperatorItem],
    /// Disclosure snapshots, keyed by [`normalize_claim`] of a hidden
    /// criterion's claim text.
    pub disclosures: &'a HashMap<String, DisclosureSnapshot>,
    /// The date to stamp the footer with.
    pub date: &'a SealDate,
}

/// Render the sealed review artifact for `input`.
///
/// Deterministic: rendering the same `input` twice yields byte-identical
/// output (task order follows `input.plan.tasks`; hidden and visible
/// criteria follow each task's own declared order; unresolved items follow
/// `input.unresolved`'s own order, which
/// [`crate::ledger::Ledger::unresolved_items`] itself derives
/// deterministically from the stored plan).
#[must_use]
pub fn render_sealed_artifact(input: &RenderInput<'_>) -> String {
    let mut out = String::new();
    render_header(&mut out, input);
    render_unresolved_section(&mut out, input);
    render_disclosed_hidden_criteria(&mut out, input);
    render_visible_criteria(&mut out, input);
    render_logs(&mut out, input.plan);
    render_footer(&mut out, input.date);
    out
}

fn render_header(out: &mut String, input: &RenderInput<'_>) {
    let disposition = if input.unresolved.is_empty()
        && !input
            .plan
            .tasks
            .iter()
            .any(|task| task.status == TaskStatus::Abandoned)
    {
        "clean: every hidden criterion resolved, no residuals"
    } else {
        "needs operator review: unresolved items below"
    };
    let _ = writeln!(
        out,
        "# Sealed review: {} — {} ({disposition})",
        input.plan.metadata.id, input.plan.metadata.title
    );
    out.push('\n');
}

// ---------------------------------------------------------------------
// (b) Unresolved items first
// ---------------------------------------------------------------------

fn render_unresolved_section(out: &mut String, input: &RenderInput<'_>) {
    out.push_str("## Unresolved Items\n\n");

    let any_abandoned = input
        .plan
        .tasks
        .iter()
        .any(|task| task.status == TaskStatus::Abandoned);

    if input.unresolved.is_empty() && !any_abandoned {
        out.push_str("None. Every hidden criterion resolved cleanly and every task completed.\n\n");
        return;
    }

    for task in &input.plan.tasks {
        let task_id = task.id.as_str();
        let items: Vec<&OperatorItem> = input
            .unresolved
            .iter()
            .filter(|item| item.task_id.as_str() == task_id)
            .collect();
        let task_abandoned = task.status == TaskStatus::Abandoned;
        if items.is_empty() && !task_abandoned {
            continue;
        }
        let _ = writeln!(out, "### Task {} — {}\n", task.id, task.title);
        for item in items {
            render_operator_item(out, task, item);
        }
        if task_abandoned {
            let reason = abandon_reason(task).unwrap_or_else(|| "no reason recorded".to_owned());
            let _ = writeln!(out, "- **Abandoned**: {reason}");
        }
        out.push('\n');
    }
}

fn abandon_reason(task: &OperatorTask) -> Option<String> {
    let evidence = task.completion_evidence.as_deref()?;
    parse_completion_evidence(evidence)
        .into_iter()
        .rev()
        .find(|entry| entry.verb == "abandoned")
        .map(|entry| entry.body)
}

fn render_operator_item(out: &mut String, task: &OperatorTask, item: &OperatorItem) {
    match &item.residual {
        Residual::AwaitingHumanJudgment { criterion_id } => {
            let claim = hidden_claim(task, criterion_id).unwrap_or("(unknown criterion)");
            let ask = hidden_criterion(task, criterion_id).and_then(|c| c.ask.as_deref());
            match ask {
                Some(ask) => {
                    let _ = writeln!(out, "- **Awaiting human judgment**: {claim} — {ask}");
                }
                None => {
                    let _ = writeln!(out, "- **Awaiting human judgment**: {claim}");
                }
            }
        }
        Residual::UndeterminedJudgment {
            criterion_id,
            evidence_needed,
        } => {
            let claim = hidden_claim(task, criterion_id).unwrap_or("(unknown criterion)");
            let _ = writeln!(
                out,
                "- **Undetermined**: {claim} — evidence needed: {}",
                evidence_needed.as_str()
            );
        }
        Residual::UncoveredChangedScope { paths, .. } => {
            let list = paths
                .iter()
                .map(crate::model::ChangedPath::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "- **Uncovered changed scope**: {list}");
        }
        Residual::JudgeDisagreement {
            criterion_id,
            first,
            second,
        } => {
            let _ = writeln!(
                out,
                "- **Judge disagreement** on `{criterion_id}`: first={}/{:?} second={}/{:?}",
                judgment_name(first.judgment()),
                first.rationale(),
                judgment_name(second.judgment()),
                second.rationale(),
            );
        }
    }
}

fn hidden_criterion<'a>(
    task: &'a OperatorTask,
    criterion_id: &crate::model::CriterionId,
) -> Option<&'a HiddenCriterion> {
    match criterion_address(criterion_id) {
        Some(CriterionAddress::Hidden { index }) => task.hidden_criteria.get(index),
        _ => None,
    }
}

fn hidden_claim<'a>(
    task: &'a OperatorTask,
    criterion_id: &crate::model::CriterionId,
) -> Option<&'a str> {
    hidden_criterion(task, criterion_id).map(|criterion| criterion.claim.as_str())
}

const fn judgment_name(judgment: &Judgment) -> &'static str {
    match judgment {
        Judgment::Pass => "pass",
        Judgment::Fail => "fail",
        Judgment::Undetermined { .. } => "undetermined",
    }
}

// ---------------------------------------------------------------------
// (c) Disclosed hidden criteria
// ---------------------------------------------------------------------

fn render_disclosed_hidden_criteria(out: &mut String, input: &RenderInput<'_>) {
    out.push_str("## Disclosed Hidden Criteria\n\n");
    let mut any = false;
    for task in &input.plan.tasks {
        if task.hidden_criteria.is_empty() {
            continue;
        }
        any = true;
        let _ = writeln!(out, "### Task {} — {}\n", task.id, task.title);
        for criterion in &task.hidden_criteria {
            render_hidden_criterion(out, criterion, input.disclosures);
        }
        out.push('\n');
    }
    if !any {
        out.push_str("None. No task in this plan carried a hidden criterion.\n\n");
    }
}

fn render_hidden_criterion(
    out: &mut String,
    criterion: &HiddenCriterion,
    disclosures: &HashMap<String, DisclosureSnapshot>,
) {
    let _ = writeln!(out, "- **Claim**: {}", criterion.claim);
    let _ = writeln!(out, "  - Evaluator: {}", evaluator_str(criterion.evaluator));
    let _ = writeln!(
        out,
        "  - Criticality: {}",
        criticality_str(criterion.criticality)
    );
    let _ = writeln!(out, "  - Why hidden: {}", criterion.why_hidden);
    let _ = writeln!(out, "  - Counterfactual: {}", criterion.counterfactual);
    let verdict = criterion
        .verdict
        .map_or_else(|| "no verdict recorded".to_owned(), verdict_str);
    let _ = writeln!(out, "  - Verdict: {verdict}");
    if let Some(rationale) = &criterion.rationale {
        let _ = writeln!(out, "  - Rationale: {rationale}");
    }
    for evidence in &criterion.evidence {
        render_hidden_evidence(out, evidence);
    }
    let key = normalize_claim(&criterion.claim);
    if let Some(snapshot) = disclosures.get(&key)
        && snapshot.burned
    {
        let _ = writeln!(
            out,
            "  - **Burned** (disclosed {} times): a worker reading merge-request \
             history would now know this criterion. The repair is refining the \
             visible instructions, not promoting the criterion to a visible \
             acceptance check.",
            snapshot.count
        );
    }
}

fn render_hidden_evidence(out: &mut String, evidence: &HiddenEvidenceRecord) {
    let _ = writeln!(
        out,
        "  - Evidence ({}): {}",
        evidence.provenance, evidence.summary
    );
}

fn verdict_str(verdict: HiddenVerdictJudgment) -> String {
    match verdict {
        HiddenVerdictJudgment::Pass => "pass".to_owned(),
        HiddenVerdictJudgment::Fail => "fail".to_owned(),
        HiddenVerdictJudgment::Undetermined => "undetermined".to_owned(),
    }
}

const fn evaluator_str(evaluator: Evaluator) -> &'static str {
    match evaluator {
        Evaluator::Automated => "automated",
        Evaluator::AgentEvaluated => "agent_evaluated",
        Evaluator::HumanJudgment => "human_judgment",
    }
}

const fn criticality_str(criticality: Criticality) -> &'static str {
    match criticality {
        Criticality::Must => "must",
        Criticality::Should => "should",
        Criticality::Nice => "nice",
    }
}

// ---------------------------------------------------------------------
// (d) Visible criteria and automated checks, routine passing checks last
// ---------------------------------------------------------------------

fn render_visible_criteria(out: &mut String, input: &RenderInput<'_>) {
    out.push_str("## Visible Criteria and Automated Checks\n\n");
    let mut any = false;
    for task in &input.plan.tasks {
        let entries = task
            .completion_evidence
            .as_deref()
            .map(parse_completion_evidence)
            .unwrap_or_default();
        if entries.is_empty() {
            continue;
        }
        any = true;
        let _ = writeln!(out, "### Task {} — {}\n", task.id, task.title);
        let mut attention = Vec::new();
        let mut routine = Vec::new();
        for entry in &entries {
            match classify_completion_evidence_entry(entry) {
                CompletionEvidenceItem::VisibleVerdict {
                    criterion_id,
                    judgment,
                    rationale,
                    evidence_needed,
                } => {
                    let line = evidence_needed.as_ref().map_or_else(
                        || format!("- `{criterion_id}`: {judgment} — {rationale}"),
                        |evidence_needed| {
                            format!(
                                "- `{criterion_id}`: {judgment} — {rationale} (evidence needed: {evidence_needed})"
                            )
                        },
                    );
                    if judgment == "pass" {
                        routine.push(line);
                    } else {
                        attention.push(line);
                    }
                }
                CompletionEvidenceItem::JudgeRunSummary { disposition, .. } => {
                    let line = format!("- judge run disposition: {disposition}");
                    if disposition == "accept" {
                        routine.push(line);
                    } else {
                        attention.push(line);
                    }
                }
                CompletionEvidenceItem::Note { verb, body } => {
                    let line = format!("- {verb}: {body}");
                    if verb == "evidence" || verb == "completed" {
                        routine.push(line);
                    } else {
                        attention.push(line);
                    }
                }
            }
        }
        for line in attention.into_iter().chain(routine) {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    if !any {
        out.push_str("None recorded.\n\n");
    }
}

// ---------------------------------------------------------------------
// `completion_evidence` parsing
// ---------------------------------------------------------------------

/// One `{date} — {verb}: {body}` record parsed out of a task's
/// `completion_evidence` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvidenceEntry {
    /// The `YYYY-MM-DD` date the record was appended under.
    pub date: String,
    /// The verb (`evidence`, `completed`, `blocked`, `reopened`,
    /// `abandoned`) the transition or append recorded.
    pub verb: String,
    /// The body text following the verb.
    pub body: String,
}

/// Split `text` (a task's whole `completion_evidence` field) into its
/// individual `{date} — {verb}: {body}` records, in document order.
///
/// Records are separated by a blank line
/// (`tftio_planner::state::append_text`'s own convention); a paragraph not
/// shaped `{date} — {verb}: {body}` is skipped rather than causing this to
/// fail, since `completion_evidence` may in principle carry
/// operator-authored free text no writer in this crate produces.
#[must_use]
pub fn parse_completion_evidence(text: &str) -> Vec<CompletionEvidenceEntry> {
    text.split("\n\n")
        .filter_map(|paragraph| {
            let (head, body) = paragraph.split_once(": ")?;
            let (date, verb) = head.split_once(" — ")?;
            Some(CompletionEvidenceEntry {
                date: date.trim().to_owned(),
                verb: verb.trim().to_owned(),
                body: body.to_owned(),
            })
        })
        .collect()
}

/// Each judged visible criterion's rendered text, judgment, and rationale,
/// in judged order -- shared by [`CompletionEvidenceItem::JudgeRunSummary`]
/// and [`parse_judge_run_summary`]'s own narrower return type.
type JudgedEntries = Vec<(String, String, String)>;

/// One classified `completion_evidence` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEvidenceItem {
    /// A visible-criterion verdict written by `src/ledger.rs`'s
    /// `render_visible_verdict`.
    VisibleVerdict {
        /// The visible criterion's id (`visible-<n>`).
        criterion_id: String,
        /// `pass`, `fail`, or `undetermined`.
        judgment: String,
        /// The rationale behind the judgment.
        rationale: String,
        /// What would resolve the judgment, when `judgment` is
        /// `undetermined`.
        evidence_needed: Option<String>,
    },
    /// A judge-run summary written by `src/tools.rs`'s
    /// `summarize_judge_run`.
    JudgeRunSummary {
        /// The run's disposition (`accept`, `reject`, `needs_operator`).
        disposition: String,
        /// Each judged visible criterion's rendered text, judgment, and
        /// rationale, in judged order.
        entries: JudgedEntries,
    },
    /// Anything else: a lifecycle verb (`completed`, `blocked`, `reopened`,
    /// `abandoned`) or free-form evidence text.
    Note {
        /// The verb the record was appended under.
        verb: String,
        /// The record's body text.
        body: String,
    },
}

/// Classify one [`CompletionEvidenceEntry`] parsed by
/// [`parse_completion_evidence`].
#[must_use]
pub fn classify_completion_evidence_entry(
    entry: &CompletionEvidenceEntry,
) -> CompletionEvidenceItem {
    if entry.verb == "evidence"
        && let Some(item) = parse_visible_verdict(&entry.body)
    {
        return item;
    }
    if entry.verb == "evidence"
        && let Some((disposition, entries)) = parse_judge_run_summary(&entry.body)
    {
        return CompletionEvidenceItem::JudgeRunSummary {
            disposition,
            entries,
        };
    }
    CompletionEvidenceItem::Note {
        verb: entry.verb.clone(),
        body: entry.body.clone(),
    }
}

fn parse_visible_verdict(body: &str) -> Option<CompletionEvidenceItem> {
    let mut lines = body.lines();
    let first = lines.next()?;
    let rest = first.strip_prefix("visible criterion ")?;
    let (criterion_id, judgment) = rest.split_once(" verdict: ")?;
    let mut rationale = String::new();
    let mut evidence_needed = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("rationale: ") {
            value.clone_into(&mut rationale);
        } else if let Some(value) = line.strip_prefix("evidence needed: ") {
            evidence_needed = Some(value.to_owned());
        }
    }
    Some(CompletionEvidenceItem::VisibleVerdict {
        criterion_id: criterion_id.to_owned(),
        judgment: judgment.to_owned(),
        rationale,
        evidence_needed,
    })
}

/// Parse a `summarize_judge_run`-shaped body into its disposition and judged
/// entries.
///
/// Returns `(String, JudgedEntries)` rather than a
/// [`CompletionEvidenceItem`]: every call site already knows it wants the
/// `JudgeRunSummary` shape, so a narrower return type means no caller (this
/// module's own [`classify_completion_evidence_entry`], or a test) has to
/// handle an enum arm this function never produces.
fn parse_judge_run_summary(body: &str) -> Option<(String, JudgedEntries)> {
    let rest = body.strip_prefix("judge run: disposition=")?;
    let (disposition, mut rest) = match rest.split_once("; ") {
        Some((disposition, rest)) => (disposition.to_owned(), rest),
        None => (rest.to_owned(), ""),
    };
    let mut entries = Vec::new();
    while let Some(after_visible) = rest.strip_prefix("visible[") {
        let Some((_, after_bracket)) = after_visible.split_once(']') else {
            break;
        };
        let Some(after_criterion) = after_bracket.trim_start().strip_prefix("criterion=") else {
            break;
        };
        let Some((criterion, after_criterion_value)) = read_debug_quoted(after_criterion) else {
            break;
        };
        let Some(after_judgment) = after_criterion_value.trim_start().strip_prefix("judgment=")
        else {
            break;
        };
        let Some((judgment, after_judgment_value)) = after_judgment.split_once(" rationale=")
        else {
            break;
        };
        let Some((rationale, remainder)) = read_debug_quoted(after_judgment_value) else {
            break;
        };
        entries.push((criterion, judgment.trim().to_owned(), rationale));
        rest = remainder
            .trim_start()
            .strip_prefix("; ")
            .unwrap_or_else(|| remainder.trim_start());
    }
    Some((disposition, entries))
}

/// Read a Rust-`{:?}`-formatted string starting at `input`'s first byte
/// (which must be an opening `"`), returning the unescaped value and the
/// remainder of `input` after the matching closing `"`.
///
/// Recognizes `\"`, `\\`, `\n`, `\t`, and `\r`; any other backslash escape
/// is passed through literally (this crate's own writers -- `src/tools.rs`'s
/// `summarize_judge_run` -- never produce one, since the text quoted is
/// always plain criterion and rationale prose).
fn read_debug_quoted(input: &str) -> Option<(String, &str)> {
    let rest = input.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Some((out, rest.get(index + 1..)?)),
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) | None => out.push('\\'),
                Some((_, other)) => {
                    out.push('\\');
                    out.push(other);
                }
            },
            other => out.push(other),
        }
    }
    None
}

// ---------------------------------------------------------------------
// (e) Decision Log and Operator Guidance Log, verbatim
// ---------------------------------------------------------------------

fn render_logs(out: &mut String, plan: &OperatorPlan) {
    out.push_str("## Decision Log\n\n");
    match &plan.decision_log {
        Some(log) if !log.trim().is_empty() => {
            out.push_str(log.trim());
            out.push('\n');
        }
        _ => out.push_str("None recorded.\n"),
    }
    out.push('\n');

    out.push_str("## Operator Guidance Log\n\n");
    match &plan.operator_guidance_log {
        Some(log) if !log.trim().is_empty() => {
            out.push_str(log.trim());
            out.push('\n');
        }
        _ => out.push_str("None recorded.\n"),
    }
    out.push('\n');
}

// ---------------------------------------------------------------------
// (f) Footer
// ---------------------------------------------------------------------

fn render_footer(out: &mut String, date: &SealDate) {
    let _ = writeln!(
        out,
        "---\n\n_Produced by silent-critic, sealed on {}._",
        date.as_str()
    );
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionEvidenceItem, DisclosureSnapshot, RenderInput, SealDate, SealDateError,
        classify_completion_evidence_entry, normalize_claim, parse_completion_evidence,
        parse_judge_run_summary, read_debug_quoted, render_sealed_artifact,
    };
    use crate::ledger::OperatorItem;
    use crate::model::{
        CriterionId, EvidenceNeeded, NonEmptyString, Residual, TaskId as CrateTaskId, Verdict,
    };
    use std::collections::HashMap;
    use std::error::Error;
    use tftio_planner::model::{
        Adr, BugMetadata, Criticality, Evaluator, ExecutionMetadata, FormatVersion,
        HiddenCriterion, OperatorPlan, OperatorTask, PlanId as PlannerPlanId, PlanMetadata,
        PlanMode, PlanStatus, Severity, SourceMetadata, SourceType, TaskGraphStatus,
        TaskId as PlannerTaskId, TaskStatus,
    };

    fn adr() -> Adr {
        Adr {
            title: "ADR".to_owned(),
            problem_statement: "problem".to_owned(),
            ticket: "ticket".to_owned(),
            discussion_summary: "discussion".to_owned(),
            context: "context".to_owned(),
            constraints: "constraints".to_owned(),
            non_goals: "non-goals".to_owned(),
            decision: "decision".to_owned(),
            alternatives_considered: "alternatives".to_owned(),
            consequences: "consequences".to_owned(),
        }
    }

    // `"PLAN-TEST"`/`"T001"`/`"T999"` are constant, always-valid literals:
    // `PlannerPlanId::parse` only rejects empty input and `PlannerTaskId::parse`
    // only rejects a shape other than `T` plus at least three digits, so
    // `.unwrap()` here can never actually panic. Isolated into one-line
    // helpers with a targeted allow, mirroring this crate's other
    // `#[allow(clippy::...)]` call sites that carry the same "structurally
    // unreachable" justification.
    #[allow(clippy::unwrap_used)]
    fn plan_id_unchecked(id: &str) -> PlannerPlanId {
        PlannerPlanId::parse(id).unwrap()
    }

    #[allow(clippy::unwrap_used)]
    fn task_id_unchecked(id: &str) -> PlannerTaskId {
        PlannerTaskId::parse(id).unwrap()
    }

    fn metadata() -> PlanMetadata {
        PlanMetadata {
            format_version: FormatVersion::V1 {
                mode: PlanMode::Single,
            },
            id: plan_id_unchecked("PLAN-TEST"),
            title: "Test plan".to_owned(),
            status: PlanStatus::Implemented,
            created_at: "2026-09-06".to_owned(),
            updated_at: "2026-09-06".to_owned(),
            owner: "test".to_owned(),
            source: SourceMetadata {
                kind: SourceType::Manual,
                url: None,
                external_id: None,
                imported_at: None,
            },
            bug: BugMetadata {
                summary: "summary".to_owned(),
                severity: Severity::Low,
                affected_area: None,
                user_impact: None,
            },
            execution: ExecutionMetadata {
                requires_operator_approval_before_implementation: true,
                requires_plan_updates_during_execution: true,
                task_graph_status: TaskGraphStatus::Complete,
            },
            project: None,
        }
    }

    fn task(id: &str, status: TaskStatus, hidden_criteria: Vec<HiddenCriterion>) -> OperatorTask {
        OperatorTask {
            id: task_id_unchecked(id),
            title: format!("Task {id}"),
            status,
            owner: None,
            depends_on: Vec::new(),
            blocks: Vec::new(),
            description: "description".to_owned(),
            work_items: Vec::new(),
            invariants: vec!["an invariant".to_owned()],
            acceptance_checks: vec!["a check".to_owned()],
            hidden_criteria,
            files: None,
            completion_evidence: None,
        }
    }

    fn plan(tasks: Vec<OperatorTask>) -> OperatorPlan {
        OperatorPlan {
            metadata: metadata(),
            adr: adr(),
            operator_guidance_log: None,
            decision_log: None,
            tasks,
            execution_protocol: None,
        }
    }

    fn hidden_criterion(
        claim: &str,
        criticality: Criticality,
        evaluator: Evaluator,
        ask: Option<&str>,
        check: Option<&str>,
    ) -> HiddenCriterion {
        HiddenCriterion {
            claim: claim.to_owned(),
            criticality,
            evaluator,
            check: check.map(str::to_owned),
            ask: ask.map(str::to_owned),
            why_hidden: "why hidden".to_owned(),
            counterfactual: "counterfactual".to_owned(),
            verdict: None,
            rationale: None,
            evidence_needed: None,
            evidence: Vec::new(),
        }
    }

    fn no_disclosures() -> HashMap<String, DisclosureSnapshot> {
        HashMap::new()
    }

    #[test]
    fn render_operator_item_falls_back_when_ask_is_absent() -> Result<(), Box<dyn Error>> {
        // `evaluator: human_judgment` with no `ask` never comes out of a
        // validated plan document (`validate_hidden_verdict` requires
        // exactly one of `check`/`ask`), but a residual can still name a
        // criterion whose `ask` field is empty defensively; the renderer
        // must not panic on it.
        let criterion = hidden_criterion(
            "a claim",
            Criticality::Must,
            Evaluator::HumanJudgment,
            None,
            None,
        );
        let one_task = task("T001", TaskStatus::Done, vec![criterion]);
        let one_plan = plan(vec![one_task]);
        let unresolved = vec![OperatorItem {
            task_id: CrateTaskId::new("T001"),
            residual: Residual::AwaitingHumanJudgment {
                criterion_id: CriterionId::new("hidden-0"),
            },
        }];
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("**Awaiting human judgment**: a claim\n"));
        Ok(())
    }

    #[test]
    fn render_operator_item_falls_back_for_an_unaddressable_criterion_id()
    -> Result<(), Box<dyn Error>> {
        let one_task = task("T001", TaskStatus::Done, Vec::new());
        let one_plan = plan(vec![one_task]);
        let unresolved = vec![OperatorItem {
            task_id: CrateTaskId::new("T001"),
            residual: Residual::AwaitingHumanJudgment {
                // `visible-0` decodes to `CriterionAddress::Visible`, which
                // `hidden_criterion` never resolves -- exercises its
                // catch-all arm.
                criterion_id: CriterionId::new("visible-0"),
            },
        }];
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("(unknown criterion)"));
        Ok(())
    }

    #[test]
    fn render_operator_item_renders_an_undetermined_disagreement() -> Result<(), Box<dyn Error>> {
        let one_task = task("T001", TaskStatus::Done, Vec::new());
        let one_plan = plan(vec![one_task]);
        let first = Verdict::new(
            CriterionId::new("hidden-0"),
            crate::model::Judgment::Undetermined {
                evidence_needed: EvidenceNeeded::new("more evidence")?,
            },
            NonEmptyString::new("unclear")?,
        );
        let second = Verdict::new(
            CriterionId::new("hidden-0"),
            crate::model::Judgment::Fail,
            NonEmptyString::new("clearly not")?,
        );
        let unresolved = vec![OperatorItem {
            task_id: CrateTaskId::new("T001"),
            residual: Residual::JudgeDisagreement {
                criterion_id: CriterionId::new("hidden-0"),
                first: Box::new(first),
                second: Box::new(second),
            },
        }];
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("first=undetermined"));
        Ok(())
    }

    #[test]
    fn render_reports_no_hidden_criteria_and_no_visible_evidence() -> Result<(), Box<dyn Error>> {
        let one_task = task("T001", TaskStatus::Done, Vec::new());
        let one_plan = plan(vec![one_task]);
        let unresolved = Vec::new();
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("No task in this plan carried a hidden criterion"));
        assert!(output.contains("None recorded.\n\n"));
        Ok(())
    }

    #[test]
    fn render_shows_a_failed_and_nice_hidden_criterion_as_burned() -> Result<(), Box<dyn Error>> {
        let mut criterion = hidden_criterion(
            "a failed claim",
            Criticality::Nice,
            Evaluator::Automated,
            None,
            Some("run it"),
        );
        criterion.verdict = Some(tftio_planner::model::HiddenVerdictJudgment::Fail);
        criterion.rationale = Some("it failed".to_owned());
        let one_task = task("T001", TaskStatus::Done, vec![criterion]);
        let one_plan = plan(vec![one_task]);
        let unresolved = Vec::new();
        let date = SealDate::new("2026-09-06")?;
        let mut disclosures = HashMap::new();
        disclosures.insert(
            normalize_claim("a failed claim"),
            DisclosureSnapshot {
                count: 4,
                burned: true,
            },
        );
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("Criticality: nice"));
        assert!(output.contains("Verdict: fail"));
        assert!(output.contains("**Burned** (disclosed 4 times)"));
        Ok(())
    }

    #[test]
    fn render_skips_a_task_with_no_completion_evidence() -> Result<(), Box<dyn Error>> {
        let one_task = task("T001", TaskStatus::Done, Vec::new());
        let one_plan = plan(vec![one_task]);
        let unresolved = Vec::new();
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        assert!(output.contains("## Visible Criteria and Automated Checks\n\nNone recorded."));
        Ok(())
    }

    #[test]
    fn render_places_an_undetermined_visible_verdict_and_a_rejected_judge_run_above_routine_lines()
    -> Result<(), Box<dyn Error>> {
        let mut one_task = task("T001", TaskStatus::Done, Vec::new());
        one_task.completion_evidence = Some(
            "2026-09-06 — evidence: visible criterion visible-0 verdict: undetermined\n\
             rationale: unclear\nevidence needed: a rerun\n\n\
             2026-09-06 — evidence: judge run: disposition=reject; visible[0] \
             criterion=\"c\" judgment=fail rationale=\"r\""
                .to_owned(),
        );
        let one_plan = plan(vec![one_task]);
        let unresolved = Vec::new();
        let date = SealDate::new("2026-09-06")?;
        let disclosures = no_disclosures();
        let output = render_sealed_artifact(&RenderInput {
            plan: &one_plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });
        let undetermined_offset = output
            .find("evidence needed: a rerun")
            .ok_or("expected the undetermined visible verdict to render")?;
        let judge_run_offset = output
            .find("judge run disposition: reject")
            .ok_or("expected the rejected judge run summary to render")?;
        assert!(undetermined_offset > 0);
        assert!(judge_run_offset > 0);
        Ok(())
    }

    #[test]
    fn seal_date_accepts_the_expected_shape() -> Result<(), Box<dyn Error>> {
        assert_eq!("2026-09-06", SealDate::new("2026-09-06")?.as_str());
        Ok(())
    }

    #[test]
    fn seal_date_rejects_malformed_input() {
        assert_eq!(Err(SealDateError), SealDate::new("2026/09/06"));
        assert_eq!(Err(SealDateError), SealDate::new(""));
        assert_eq!(Err(SealDateError), SealDate::new("2026-09-06-extra"));
    }

    #[test]
    fn seal_date_error_displays_a_message() {
        assert_eq!(
            "seal date must be shaped YYYY-MM-DD",
            SealDateError.to_string()
        );
    }

    #[test]
    fn normalize_claim_trims_and_lowercases() {
        assert_eq!("hello world", normalize_claim("  Hello WORLD  "));
    }

    #[test]
    fn parse_completion_evidence_splits_paragraphs() -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — evidence: plain note\n\n2026-09-06 — completed: done";
        let entries = parse_completion_evidence(text);
        assert_eq!(2, entries.len());
        let first = entries.first().ok_or("expected a first entry")?;
        let second = entries.get(1).ok_or("expected a second entry")?;
        assert_eq!("evidence", first.verb);
        assert_eq!("plain note", first.body);
        assert_eq!("completed", second.verb);
        assert_eq!("done", second.body);
        Ok(())
    }

    #[test]
    fn parse_completion_evidence_skips_unshaped_paragraphs() -> Result<(), Box<dyn Error>> {
        let text = "not shaped at all\n\n2026-09-06 — evidence: ok";
        let entries = parse_completion_evidence(text);
        assert_eq!(1, entries.len());
        let first = entries.first().ok_or("expected an entry")?;
        assert_eq!("ok", first.body);
        Ok(())
    }

    fn only_entry(text: &str) -> Result<super::CompletionEvidenceEntry, Box<dyn Error>> {
        parse_completion_evidence(text)
            .into_iter()
            .next()
            .ok_or_else(|| "expected at least one parsed entry".into())
    }

    #[test]
    fn classify_recognizes_a_visible_verdict() -> Result<(), Box<dyn Error>> {
        let text =
            "2026-09-06 — evidence: visible criterion visible-0 verdict: pass\nrationale: because";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::VisibleVerdict {
                criterion_id: "visible-0".to_owned(),
                judgment: "pass".to_owned(),
                rationale: "because".to_owned(),
                evidence_needed: None,
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_recognizes_a_visible_verdict_with_evidence_needed() -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — evidence: visible criterion visible-0 verdict: undetermined\n\
                     rationale: unclear\nevidence needed: a rerun";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::VisibleVerdict {
                criterion_id: "visible-0".to_owned(),
                judgment: "undetermined".to_owned(),
                rationale: "unclear".to_owned(),
                evidence_needed: Some("a rerun".to_owned()),
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_recognizes_a_judge_run_summary() -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — evidence: judge run: disposition=accept; visible[0] \
                     criterion=\"do the thing\" judgment=pass rationale=\"looks fine\"";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::JudgeRunSummary {
                disposition: "accept".to_owned(),
                entries: vec![(
                    "do the thing".to_owned(),
                    "pass".to_owned(),
                    "looks fine".to_owned()
                )],
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_recognizes_a_judge_run_summary_with_multiple_visible_entries()
    -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — evidence: judge run: disposition=accept; visible[0] \
                     criterion=\"first\" judgment=pass rationale=\"ok\"; visible[1] \
                     criterion=\"second\" judgment=fail rationale=\"nope\"";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::JudgeRunSummary {
                disposition: "accept".to_owned(),
                entries: vec![
                    ("first".to_owned(), "pass".to_owned(), "ok".to_owned()),
                    ("second".to_owned(), "fail".to_owned(), "nope".to_owned()),
                ],
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_falls_back_to_a_note_for_anything_else() -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — completed: all done";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::Note {
                verb: "completed".to_owned(),
                body: "all done".to_owned(),
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_falls_back_to_a_note_when_visible_verdict_shape_is_broken()
    -> Result<(), Box<dyn Error>> {
        let text = "2026-09-06 — evidence: visible criterion but no verdict marker";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::Note {
                verb: "evidence".to_owned(),
                body: "visible criterion but no verdict marker".to_owned(),
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    #[test]
    fn classify_falls_back_to_a_note_when_judge_run_shape_is_broken() -> Result<(), Box<dyn Error>>
    {
        let text = "2026-09-06 — evidence: judge run: disposition=accept; visible[oops";
        let entry = only_entry(text)?;
        assert_eq!(
            CompletionEvidenceItem::JudgeRunSummary {
                disposition: "accept".to_owned(),
                entries: Vec::new(),
            },
            classify_completion_evidence_entry(&entry)
        );
        Ok(())
    }

    fn expect_judge_run_entries(body: &str) -> super::JudgedEntries {
        parse_judge_run_summary(body)
            .map(|(_, entries)| entries)
            .unwrap_or_default()
    }

    #[test]
    fn parse_judge_run_summary_with_no_visible_entries_at_all() {
        let entries = expect_judge_run_entries("judge run: disposition=accept");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_judge_run_summary_stops_at_a_missing_criterion_marker() {
        let entries =
            expect_judge_run_entries("judge run: disposition=accept; visible[0] nope=here");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_judge_run_summary_stops_at_an_unquoted_criterion() {
        let entries = expect_judge_run_entries(
            "judge run: disposition=accept; visible[0] criterion=unquoted",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_judge_run_summary_stops_at_a_missing_judgment_marker() {
        let entries = expect_judge_run_entries(
            "judge run: disposition=accept; visible[0] criterion=\"c\" nope=here",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_judge_run_summary_stops_at_a_missing_rationale_marker() {
        let entries = expect_judge_run_entries(
            "judge run: disposition=accept; visible[0] criterion=\"c\" judgment=pass nope=here",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_judge_run_summary_stops_at_an_unquoted_rationale() {
        let entries = expect_judge_run_entries(
            "judge run: disposition=accept; visible[0] criterion=\"c\" judgment=pass rationale=unquoted",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn read_debug_quoted_unescapes_every_recognized_sequence() -> Result<(), Box<dyn Error>> {
        let input = r#""quote:\" backslash:\\ newline:\n tab:\t cr:\r other:\z" and the rest"#;
        let (value, rest) = read_debug_quoted(input).ok_or("expected a parsed value")?;
        assert_eq!(
            "quote:\" backslash:\\ newline:\n tab:\t cr:\r other:\\z",
            value
        );
        assert_eq!(" and the rest", rest);
        Ok(())
    }

    #[test]
    fn read_debug_quoted_handles_a_trailing_backslash_with_nothing_after_it() {
        // Not producible by any writer in this crate (a well-formed
        // Debug-quoted string never ends mid-escape), but the parser must
        // still terminate rather than index out of bounds.
        assert_eq!(None, read_debug_quoted("\"trailing\\"));
    }

    #[test]
    fn read_debug_quoted_rejects_input_with_no_opening_quote() {
        assert_eq!(None, read_debug_quoted("no quote here"));
    }

    /// Same fixture `tests/seal.rs`'s and `src/seal.rs`'s own lib-instance
    /// tests use: a closed task graph with a human-judgment criterion, an
    /// already-undetermined one, a passing automated one, and an abandoned
    /// task, so this one test can drive a *real* additional hidden-verdict
    /// write through `Ledger` and then render the result.
    const FULL_PLAN: &str = include_str!("../tests/fixtures/2026-09-06-sealed-full-plan.md");

    #[test]
    fn render_end_to_end_over_a_ledger_written_plan() -> Result<(), Box<dyn Error>> {
        use crate::ledger::Ledger;
        use crate::model::{Evidence, EvidenceId, EvidenceProvenance};

        let fixture = crate::test_support::set_up(FULL_PLAN)?;
        let ledger = Ledger::new(&fixture.store, &fixture.repo_start, &fixture.plan_id);

        // T002's `hidden-0` (`human_judgment`, no verdict in the fixture
        // YAML) becomes undetermined through a real `Ledger` write, from
        // this crate's own `--cfg test` build -- not baked into the
        // fixture text the way `tests/seal.rs`'s equivalent checks are.
        let verdict = Verdict::new(
            CriterionId::new("hidden-0"),
            crate::model::Judgment::Undetermined {
                evidence_needed: EvidenceNeeded::new("a transcript excerpt naming the check")?,
            },
            NonEmptyString::new("the operator could not tell from the transcript alone")?,
        );
        let evidence = Evidence::new(
            EvidenceId::new("ev-lib-render-test"),
            EvidenceProvenance::OperatorObserved,
            NonEmptyString::new("operator reviewed the transcript and could not settle it")?,
        );
        let task_two = CrateTaskId::new("T002");
        let evidence_slice = std::slice::from_ref(&evidence);
        ledger.record_hidden_verdict(&task_two, 0, &verdict, evidence_slice)?;

        let plan_path = fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?;
        let source = std::fs::read_to_string(&plan_path)?;
        let plan = tftio_planner::parse_markdown(&source)?;
        let unresolved = ledger.unresolved_items()?;

        // The claim just re-verdicted above, reported burned: a claim
        // disclosed repeatedly enough that a worker reading merge-request
        // history would now know it.
        let mut disclosures = HashMap::new();
        disclosures.insert(
            normalize_claim("the worker did not weaken any existing check"),
            DisclosureSnapshot {
                count: 5,
                burned: true,
            },
        );

        let date = SealDate::new("2026-09-06")?;
        let output = render_sealed_artifact(&RenderInput {
            plan: &plan,
            unresolved: &unresolved,
            disclosures: &disclosures,
            date: &date,
        });

        // Real content, in the residual-first order T011 requires.
        let undetermined_offset = output
            .find("evidence needed: a transcript excerpt naming the check")
            .ok_or("expected the ledger-written undetermined item to render")?;
        let abandoned_offset = output
            .find("**Abandoned**")
            .ok_or("expected the abandoned task to render")?;
        let burned_offset = output
            .find("**Burned** (disclosed 5 times)")
            .ok_or("expected the burned criterion to render")?;
        // A routine passing *visible* check, in the "Visible Criteria and
        // Automated Checks" section -- the section the residual-first
        // invariant requires every unresolved item to precede.
        let passing_offset = output
            .find("the ground looked fine")
            .ok_or("expected a routine passing visible check to render")?;
        assert!(undetermined_offset < passing_offset);
        assert!(abandoned_offset < passing_offset);
        assert!(
            burned_offset > undetermined_offset,
            "burned marker renders under its own criterion, in the disclosed-criteria section, after the unresolved-items section"
        );

        // The `completion_evidence` parser, over this same real plan's own
        // task completion evidence (not a hand-written literal).
        let task_one = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "T001")
            .ok_or("expected T001 in the sealed-full-plan fixture")?;
        let evidence_text = task_one
            .completion_evidence
            .as_deref()
            .ok_or("expected T001 to carry completion evidence")?;
        let entries = parse_completion_evidence(evidence_text);
        assert!(!entries.is_empty());
        let first_entry = entries
            .first()
            .ok_or("expected at least one completion-evidence entry")?;
        assert_eq!(
            CompletionEvidenceItem::VisibleVerdict {
                criterion_id: "visible-0".to_owned(),
                judgment: "pass".to_owned(),
                rationale: "the ground looked fine".to_owned(),
                evidence_needed: None,
            },
            classify_completion_evidence_entry(first_entry)
        );

        Ok(())
    }
}
