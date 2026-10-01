//! The automated evaluator: run a criterion's shell check with a bounded
//! timeout, an explicit working directory, and an explicit environment, and
//! record the outcome as verbatim, tool-authored evidence.
//!
//! No model is involved on this path. The check runs as `sh -c <check>`
//! (the shell binary is caller-supplied, part of [`CheckSpec`], never
//! defaulted here), with an environment built from scratch by
//! [`std::process::Command::env_clear`] plus the explicit pairs the caller
//! supplies — `PATH` included, since nothing here reads the ambient process
//! environment. The timeout is enforced by polling
//! [`std::process::Child::try_wait`] and killing the child on expiry; this
//! module spawns no async runtime.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{
    Criterion, CriterionId, EmptyStringError, EvaluatorKind, Evidence, EvidenceId,
    EvidenceProvenance, NonEmptyString, TaskId,
};

/// How long to sleep between polls of the child process's status while
/// waiting for it to finish or for its timeout to expire.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A bounded timeout for an automated check, carried as a typed newtype
/// rather than a bare [`Duration`] so a call site cannot confuse it with any
/// other duration in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckTimeout(Duration);

impl CheckTimeout {
    /// Build a timeout from a [`Duration`].
    #[must_use]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// The underlying duration.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

/// An explicit, ordered environment for a check: name/value pairs applied on
/// top of [`std::process::Command::env_clear`], never the ambient process
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckEnvironment {
    pairs: Vec<(String, String)>,
}

impl CheckEnvironment {
    /// An empty environment: after `env_clear`, the child sees nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    /// Append one name/value pair, in builder style.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.pairs.push((name.into(), value.into()));
        self
    }

    /// The name/value pairs, in the order they were added.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// The variable names carried by this environment, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.pairs.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// The full specification of an automated check to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSpec {
    /// The shell command text, run as `<shell> -c <check>`.
    pub check: String,
    /// The shell binary to invoke. Caller-supplied; this module never
    /// defaults it (that default lives at the process edge).
    pub shell: PathBuf,
    /// The task worktree: the working directory used when `working_dir` is
    /// not set.
    pub worktree: PathBuf,
    /// An explicit override for the working directory, taking precedence
    /// over `worktree` when present.
    pub working_dir: Option<PathBuf>,
    /// The check's explicit environment.
    pub environment: CheckEnvironment,
    /// The bounded timeout for the check.
    pub timeout: CheckTimeout,
}

impl CheckSpec {
    /// The effective working directory: the explicit override if set, else
    /// the task worktree.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        self.working_dir.as_deref().unwrap_or(&self.worktree)
    }
}

/// Why a check's exit was not a clean success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitFailure {
    /// The process exited with this non-zero status code.
    Code(i32),
    /// The process was terminated by this signal.
    Signal(i32),
}

/// The recorded outcome of running an automated check: a closed enum with
/// three distinguishable variants, dispatched exhaustively wherever it is
/// matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckOutcome {
    /// The check exited zero.
    Passed,
    /// The check exited non-zero, or was terminated by a signal.
    Failed(ExitFailure),
    /// The check did not finish within its timeout and was killed.
    TimedOut(CheckTimeout),
}

/// The full recorded result of running an automated check: everything an
/// operator needs to read the outcome without re-running anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// The shell command text that was run.
    pub check: String,
    /// The working directory the check ran in.
    pub working_dir: PathBuf,
    /// The explicit environment the check ran with.
    pub environment: CheckEnvironment,
    /// The recorded outcome.
    pub outcome: CheckOutcome,
    /// Captured standard output, verbatim (including whatever was captured
    /// before a timeout killed the process).
    pub stdout: Vec<u8>,
    /// Captured standard error, verbatim (including whatever was captured
    /// before a timeout killed the process).
    pub stderr: Vec<u8>,
    /// Wall-clock time the check ran for.
    pub wall_time: Duration,
}

/// A spawn-time failure running an automated check.
///
/// Covers a missing shell binary, a bad working directory, or any other
/// reason the process could not be started or waited on. This is distinct
/// from [`CheckOutcome`], which records how a *successfully started* check
/// finished.
#[derive(Debug, Error)]
pub enum AutomatedCheckError {
    /// The check's shell process could not be spawned.
    #[error("spawning `{shell}` in {working_dir}: {source}", shell = shell.display(), working_dir = working_dir.display())]
    Spawn {
        /// The shell binary that could not be spawned.
        shell: PathBuf,
        /// The working directory the spawn was attempted from.
        working_dir: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Waiting on the spawned check process failed.
    #[error("waiting for the check process: {source}")]
    Wait {
        /// The underlying I/O error.
        #[from]
        source: std::io::Error,
    },
}

/// Run an automated check to completion (or until its timeout expires) and
/// return the full recorded result.
///
/// # Errors
///
/// Returns [`AutomatedCheckError::Spawn`] when the shell binary named in
/// `spec.shell` cannot be spawned (for example, because it does not exist,
/// or because `spec.working_dir()` does not exist), or
/// [`AutomatedCheckError::Wait`] when the operating system reports a failure
/// waiting on the spawned process. Neither case produces a [`CheckOutcome`]:
/// a check that never started has no outcome to record.
pub fn run_check(spec: &CheckSpec) -> Result<CheckResult, AutomatedCheckError> {
    let working_dir = spec.working_dir().to_path_buf();

    let mut command = Command::new(&spec.shell);
    command.arg("-c").arg(&spec.check);
    command.current_dir(&working_dir);
    command.env_clear();
    for (name, value) in spec.environment.pairs() {
        command.env(name, value);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Make the check the leader of its own process group so a timeout
        // can kill the whole group, not just this immediate `sh` process —
        // a pipeline member, a backgrounded job, or any other descendant
        // would otherwise keep the stdout/stderr pipes open indefinitely
        // after `sh` itself is gone.
        command.process_group(0);
    }

    let start = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|source| AutomatedCheckError::Spawn {
            shell: spec.shell.clone(),
            working_dir: working_dir.clone(),
            source,
        })?;

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| spawn_drain(pipe, Arc::clone(&stdout_buf)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| spawn_drain(pipe, Arc::clone(&stderr_buf)));

    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break outcome_from_status(status);
        }
        if start.elapsed() >= spec.timeout.as_duration() {
            #[cfg(unix)]
            kill_process_group(spec, child.id());
            // Best-effort: the child (and, on Unix, its whole process
            // group) is already on its way out; a failure here would only
            // matter if we needed to prove it died, and the recorded
            // outcome already says so.
            let _ = child.kill();
            let _ = child.wait();
            break CheckOutcome::TimedOut(spec.timeout);
        }
        thread::sleep(POLL_INTERVAL);
    };

    let wall_time = start.elapsed();

    // The reader threads exit once their pipe reaches EOF, which happens
    // both on normal exit and once the killed child's pipes close.
    if let Some(handle) = stdout_reader {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_reader {
        let _ = handle.join();
    }

    // A poisoned lock still holds a perfectly good `Vec<u8>` — the drain
    // thread never panics while holding the lock (see `spawn_drain`), so
    // poisoning cannot actually happen here, but `PoisonError::into_inner`
    // recovers the guard either way rather than needing a branch for it.
    let stdout = stdout_buf
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let stderr = stderr_buf
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();

    Ok(CheckResult {
        check: spec.check.clone(),
        working_dir,
        environment: spec.environment.clone(),
        outcome,
        stdout,
        stderr,
        wall_time,
    })
}

/// Send `SIGKILL` to the whole process group led by `pid`.
///
/// `pid` is the check's own process group (see `process_group(0)` at spawn
/// time in [`run_check`]), so this reaches descendants — pipeline members,
/// backgrounded jobs — that killing just the immediate `sh` child would
/// leave running, still holding the stdout/stderr pipes open.
///
/// This shells out to `kill` through the caller-supplied shell rather than
/// linking `libc` for one syscall, with its own explicit, `env_clear`-then-
/// pairs environment for the same reason [`run_check`] never inherits the
/// ambient environment. Best-effort: this can fail to even spawn (a
/// vanishingly unlikely resource-exhaustion case), in which case the
/// `child.kill()` at the call site still reaches the immediate child.
#[cfg(unix)]
fn kill_process_group(spec: &CheckSpec, pid: u32) {
    let mut kill_command = Command::new(&spec.shell);
    kill_command.arg("-c").arg(format!("kill -KILL -{pid}"));
    kill_command.env_clear();
    for (name, value) in spec.environment.pairs() {
        kill_command.env(name, value);
    }
    kill_command.stdin(Stdio::null());
    kill_command.stdout(Stdio::null());
    kill_command.stderr(Stdio::null());
    let _ = kill_command.status();
}

/// Spawn a thread that drains `pipe` to EOF into `buf`.
///
/// Draining happens on a dedicated thread so the child is never blocked on a
/// full pipe buffer while the caller is only polling its exit status. A read
/// error part-way through (for example, because the child was just killed
/// and its pipe closed abruptly) simply means less output was captured; it
/// is not a failure of the check and is intentionally not propagated — the
/// caller records whatever was captured, verbatim.
fn spawn_drain(
    mut pipe: impl Read + Send + 'static,
    buf: Arc<Mutex<Vec<u8>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut collected = Vec::new();
        let _ = pipe.read_to_end(&mut collected);
        if let Ok(mut guard) = buf.lock() {
            *guard = collected;
        }
    })
}

/// Translate a finished process's [`ExitStatus`] into a [`CheckOutcome`].
fn outcome_from_status(status: ExitStatus) -> CheckOutcome {
    if status.success() {
        return CheckOutcome::Passed;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return CheckOutcome::Failed(ExitFailure::Signal(signal));
        }
    }
    CheckOutcome::Failed(ExitFailure::Code(status.code().unwrap_or(-1)))
}

impl CheckResult {
    /// Render this result as tool-authored [`Evidence`] for the given
    /// criterion and task, stamped [`EvidenceProvenance::ToolAuthored`].
    ///
    /// Every field this result carries — the check text, the working
    /// directory, the environment's variable names, the outcome, and
    /// captured stdout/stderr verbatim (converted from bytes with
    /// [`String::from_utf8_lossy`]; any invalid UTF-8 is replaced rather
    /// than dropped, never re-encoded or condensed) — is written into the
    /// evidence text as-is. Nothing here is summarized by a model.
    ///
    /// Environment *values* are recorded verbatim alongside their names by
    /// default: the operator constructed this environment deliberately, and
    /// redacting it would hide exactly the configuration an operator reading
    /// this evidence needs to see.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyStringError`] when the rendered text is empty or
    /// all-whitespace. In practice this cannot happen — `render_summary`
    /// always begins with a fixed, non-blank header — but the check text,
    /// working directory, and environment are caller-supplied, so this is
    /// modeled as a real (if practically unreachable) recoverable error
    /// rather than asserted away.
    pub fn to_evidence(
        &self,
        id: EvidenceId,
        criterion_id: &CriterionId,
        task_id: &TaskId,
    ) -> Result<Evidence, EmptyStringError> {
        let summary = NonEmptyString::new(render_summary(self, criterion_id, task_id))?;
        Ok(Evidence::new(id, EvidenceProvenance::ToolAuthored, summary))
    }
}

/// Render a [`CheckResult`] as verbatim, human-readable text.
fn render_summary(result: &CheckResult, criterion_id: &CriterionId, task_id: &TaskId) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("automated check evidence\n");
    // `write!` to a `String` never fails; these `let _` discard the
    // infallible `Ok(())` rather than a genuine error.
    let _ = writeln!(out, "task: {task_id}");
    let _ = writeln!(out, "criterion: {criterion_id}");
    let _ = writeln!(out, "check: {}", result.check);
    let _ = writeln!(out, "working directory: {}", result.working_dir.display());
    out.push_str("environment:\n");
    for (name, value) in result.environment.pairs() {
        let _ = writeln!(out, "  {name}={value}");
    }
    let _ = writeln!(out, "outcome: {}", describe_outcome(result.outcome));
    let _ = writeln!(out, "wall time: {:?}", result.wall_time);
    out.push_str("--- stdout ---\n");
    out.push_str(&String::from_utf8_lossy(&result.stdout));
    out.push_str("\n--- stderr ---\n");
    out.push_str(&String::from_utf8_lossy(&result.stderr));
    out
}

/// Describe a [`CheckOutcome`] in one line, dispatched exhaustively.
fn describe_outcome(outcome: CheckOutcome) -> String {
    match outcome {
        CheckOutcome::Passed => "passed".to_string(),
        CheckOutcome::Failed(ExitFailure::Code(code)) => format!("failed (exit code {code})"),
        CheckOutcome::Failed(ExitFailure::Signal(signal)) => {
            format!("failed (signal {signal})")
        }
        CheckOutcome::TimedOut(timeout) => {
            format!("timed out after {:?}", timeout.as_duration())
        }
    }
}

/// A criterion whose [`EvaluatorKind`] is not [`EvaluatorKind::Automated`]
/// was handed to [`evaluate`].
#[derive(Debug, Error, PartialEq, Eq)]
#[error("criterion {0} is not evaluated automatically")]
pub struct NotAutomatedError(CriterionId);

/// Errors from [`evaluate`]: the criterion was not automated, running its
/// check failed to even start, or the rendered evidence text was rejected.
#[derive(Debug, Error)]
pub enum EvaluateAutomatedError {
    /// The criterion's evaluator is not [`EvaluatorKind::Automated`].
    #[error(transparent)]
    NotAutomated(#[from] NotAutomatedError),
    /// The check could not be run to completion.
    #[error(transparent)]
    Check(#[from] AutomatedCheckError),
    /// The rendered evidence text was empty or all-whitespace.
    #[error(transparent)]
    Evidence(#[from] EmptyStringError),
}

/// Run `criterion`'s automated check for `task_id` and record the result as
/// tool-authored evidence.
///
/// # Errors
///
/// Returns [`EvaluateAutomatedError::NotAutomated`] when `criterion`'s
/// evaluator is not [`EvaluatorKind::Automated`],
/// [`EvaluateAutomatedError::Check`] when the check's shell process could
/// not be spawned or waited on, or [`EvaluateAutomatedError::Evidence`] when
/// the rendered evidence text could not be recorded (see
/// [`CheckResult::to_evidence`]).
pub fn evaluate(
    criterion: &Criterion,
    task_id: &TaskId,
    spec: &CheckSpec,
    evidence_id: EvidenceId,
) -> Result<Evidence, EvaluateAutomatedError> {
    if criterion.evaluator() != EvaluatorKind::Automated {
        return Err(NotAutomatedError(criterion.id().clone()).into());
    }
    let result = run_check(spec)?;
    let evidence = result.to_evidence(evidence_id, criterion.id(), task_id)?;
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::{
        AutomatedCheckError, CheckEnvironment, CheckOutcome, CheckSpec, CheckTimeout,
        EvaluateAutomatedError, ExitFailure, evaluate, run_check,
    };
    use crate::model::{
        Criterion, CriterionId, CriterionVisibility, EvaluatorKind, EvidenceId, EvidenceProvenance,
        TaskId,
    };
    use std::error::Error;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn spec(dir: &std::path::Path, check: &str, timeout: Duration) -> CheckSpec {
        CheckSpec {
            check: check.to_string(),
            shell: PathBuf::from("/bin/sh"),
            worktree: dir.to_path_buf(),
            working_dir: None,
            environment: CheckEnvironment::new().with("PATH", "/usr/bin:/bin"),
            timeout: CheckTimeout::new(timeout),
        }
    }

    /// The message of `result`'s error, or an empty string if it is `Ok`.
    fn error_message<T, E: std::fmt::Display>(result: &Result<T, E>) -> String {
        result
            .as_ref()
            .err()
            .map_or_else(String::new, ToString::to_string)
    }

    #[test]
    fn a_passing_check_is_recorded_as_passed() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let result = run_check(&spec(dir.path(), "exit 0", Duration::from_secs(5)))?;
        assert_eq!(CheckOutcome::Passed, result.outcome);
        Ok(())
    }

    #[test]
    fn a_failing_check_is_recorded_as_failed_with_its_exit_code() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let result = run_check(&spec(dir.path(), "exit 7", Duration::from_secs(5)))?;
        assert_eq!(CheckOutcome::Failed(ExitFailure::Code(7)), result.outcome);
        Ok(())
    }

    #[test]
    fn a_check_killed_by_a_signal_is_recorded_as_failed_with_its_signal()
    -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        // The shell's `$$` is its own PID; sending SIGKILL to it terminates
        // the check process by signal rather than by a normal exit.
        let result = run_check(&spec(dir.path(), "kill -9 $$", Duration::from_secs(5)))?;
        assert_eq!(CheckOutcome::Failed(ExitFailure::Signal(9)), result.outcome);
        Ok(())
    }

    #[test]
    fn a_hanging_check_is_recorded_as_timed_out_and_the_child_is_killed()
    -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let result = run_check(&spec(dir.path(), "sleep 30", timeout))?;
        let elapsed = started.elapsed();

        assert_eq!(
            CheckOutcome::TimedOut(CheckTimeout::new(timeout)),
            result.outcome
        );
        // If the child were not actually killed, this would have blocked for
        // the full 30-second sleep instead of returning shortly after the
        // 200ms timeout.
        assert!(
            elapsed < Duration::from_secs(10),
            "expected the timed-out child to be killed promptly, took {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn a_check_with_a_pipeline_descendant_is_fully_killed_on_timeout() -> Result<(), Box<dyn Error>>
    {
        let dir = tempfile::tempdir()?;
        // A bare `sleep N` is exec'd directly by `sh -c`, so it *is* the
        // immediate child and killing just that child is enough — it would
        // not catch the bug this test targets. A pipeline forks `sleep N`
        // off as a genuine descendant of the shell, with its own stdout fd
        // (piped from `sh`, not `sh`'s own stdout), so a fix that only
        // kills the immediate `sh` child leaves it running and its pipe fd
        // open. The marker is unique in this file, and it also carries this
        // test process's pid: `mise run ci` runs the suite twice at once (the
        // prek test hook and the coverage run), so a fixed marker lets one
        // run's process-absence check see the other run's live `sleep`.
        let marker = format!("sleep 613{}", std::process::id());
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let result = run_check(&spec(dir.path(), &format!("{marker} | cat"), timeout))?;
        let elapsed = started.elapsed();

        assert_eq!(
            CheckOutcome::TimedOut(CheckTimeout::new(timeout)),
            result.outcome
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "expected the timed-out pipeline to be killed promptly, took {elapsed:?}"
        );

        let descendant_survived = Command::new("pgrep")
            .arg("-f")
            .arg(&marker)
            .status()?
            .success();
        assert!(
            !descendant_survived,
            "expected no `{marker}` descendant to survive the timeout"
        );
        Ok(())
    }

    #[test]
    fn a_zero_duration_timeout_times_out_immediately() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let timeout = Duration::from_secs(0);
        let result = run_check(&spec(dir.path(), "sleep 1", timeout))?;
        assert_eq!(
            CheckOutcome::TimedOut(CheckTimeout::new(timeout)),
            result.outcome
        );
        Ok(())
    }

    #[test]
    fn the_three_outcomes_are_distinguishable() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let passed = run_check(&spec(dir.path(), "exit 0", Duration::from_secs(5)))?.outcome;
        let failed = run_check(&spec(dir.path(), "exit 1", Duration::from_secs(5)))?.outcome;
        let timed_out =
            run_check(&spec(dir.path(), "sleep 30", Duration::from_millis(200)))?.outcome;

        assert_ne!(passed, failed);
        assert_ne!(passed, timed_out);
        assert_ne!(failed, timed_out);
        Ok(())
    }

    #[test]
    fn stdout_and_stderr_are_captured_verbatim() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let result = run_check(&spec(
            dir.path(),
            "printf 'hello-out'; printf 'hello-err' 1>&2",
            Duration::from_secs(5),
        ))?;
        assert_eq!(b"hello-out".as_slice(), result.stdout.as_slice());
        assert_eq!(b"hello-err".as_slice(), result.stderr.as_slice());
        Ok(())
    }

    #[test]
    fn working_directory_defaults_to_the_task_worktree() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let expected = dir.path().canonicalize()?;
        let result = run_check(&spec(dir.path(), "pwd", Duration::from_secs(5)))?;
        let printed = String::from_utf8_lossy(&result.stdout);
        assert_eq!(expected, PathBuf::from(printed.trim()));
        assert_eq!(dir.path(), result.working_dir);
        Ok(())
    }

    #[test]
    fn an_explicit_working_directory_override_is_honored() -> Result<(), Box<dyn Error>> {
        let worktree = tempfile::tempdir()?;
        let override_dir = tempfile::tempdir()?;
        let expected = override_dir.path().canonicalize()?;

        let mut check = spec(worktree.path(), "pwd", Duration::from_secs(5));
        check.working_dir = Some(override_dir.path().to_path_buf());

        let result = run_check(&check)?;
        let printed = String::from_utf8_lossy(&result.stdout);
        assert_eq!(expected, PathBuf::from(printed.trim()));
        assert_eq!(override_dir.path(), result.working_dir);
        Ok(())
    }

    #[test]
    fn the_environment_is_not_inherited_from_the_ambient_process() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        // `cargo`/`cargo nextest` set `CARGO_MANIFEST_DIR` as a real
        // environment variable on this very test process. If the check
        // inherited the ambient environment, it would show up in the
        // child's `env` output; `env_clear` means it must not.
        let mut check = spec(dir.path(), "env", Duration::from_secs(5));
        check.environment = check
            .environment
            .with("SILENT_CRITIC_TEST_MARKER", "present");

        let result = run_check(&check)?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("SILENT_CRITIC_TEST_MARKER=present"));
        assert!(
            !stdout.contains("CARGO_MANIFEST_DIR"),
            "ambient CARGO_MANIFEST_DIR leaked into the check's environment: {stdout}"
        );
        Ok(())
    }

    #[test]
    fn a_missing_shell_is_a_typed_spawn_error() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let mut check = spec(dir.path(), "exit 0", Duration::from_secs(5));
        check.shell = dir.path().join("no-such-shell-binary");

        let result = run_check(&check);
        assert!(
            matches!(result, Err(AutomatedCheckError::Spawn { .. })),
            "expected a spawn error for a missing shell, got: {result:?}"
        );
        let message = error_message(&result);
        assert!(
            message.contains("no-such-shell-binary"),
            "expected the error to name the missing shell, got: {message}"
        );
        Ok(())
    }

    #[test]
    fn a_missing_working_directory_is_a_typed_spawn_error() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("does-not-exist");
        let mut check = spec(dir.path(), "exit 0", Duration::from_secs(5));
        check.working_dir = Some(missing.clone());

        let result = run_check(&check);
        assert!(
            matches!(result, Err(AutomatedCheckError::Spawn { .. })),
            "expected a spawn error for a missing cwd, got: {result:?}"
        );
        let message = error_message(&result);
        assert!(
            message.contains(&missing.display().to_string()),
            "expected the error to name the missing directory, got: {message}"
        );
        Ok(())
    }

    fn automated_criterion() -> Criterion {
        Criterion::new(
            CriterionId::new("crit-1"),
            EvaluatorKind::Automated,
            CriterionVisibility::Visible,
        )
    }

    #[test]
    fn evaluate_records_tool_authored_evidence_naming_the_criterion_and_task()
    -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let criterion = automated_criterion();
        let task_id = TaskId::new("task-1");
        let check = spec(
            dir.path(),
            "printf out; printf err 1>&2",
            Duration::from_secs(5),
        );

        let evidence = evaluate(&criterion, &task_id, &check, EvidenceId::new("ev-1"))?;

        assert_eq!("ev-1", evidence.id().as_str());
        assert_eq!(EvidenceProvenance::ToolAuthored, evidence.provenance());
        let summary = evidence.summary();
        assert!(summary.contains("task-1"));
        assert!(summary.contains("crit-1"));
        assert!(summary.contains("printf out; printf err 1>&2"));
        assert!(summary.contains(&dir.path().display().to_string()));
        assert!(summary.contains("PATH=/usr/bin:/bin"));
        assert!(summary.contains("passed"));
        assert!(summary.contains("--- stdout ---\nout"));
        assert!(summary.contains("--- stderr ---\nerr"));
        Ok(())
    }

    #[test]
    fn evaluate_records_a_failed_outcome_by_exit_code() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let criterion = automated_criterion();
        let task_id = TaskId::new("task-1");
        let check = spec(dir.path(), "exit 3", Duration::from_secs(5));

        let evidence = evaluate(&criterion, &task_id, &check, EvidenceId::new("ev-2"))?;
        assert!(evidence.summary().contains("failed (exit code 3)"));
        Ok(())
    }

    #[test]
    fn evaluate_records_a_failed_outcome_by_signal() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let criterion = automated_criterion();
        let task_id = TaskId::new("task-1");
        let check = spec(dir.path(), "kill -9 $$", Duration::from_secs(5));

        let evidence = evaluate(&criterion, &task_id, &check, EvidenceId::new("ev-2b"))?;
        assert!(evidence.summary().contains("failed (signal 9)"));
        Ok(())
    }

    #[test]
    fn evaluate_records_a_timed_out_outcome() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let criterion = automated_criterion();
        let task_id = TaskId::new("task-1");
        let check = spec(dir.path(), "sleep 30", Duration::from_millis(200));

        let evidence = evaluate(&criterion, &task_id, &check, EvidenceId::new("ev-3"))?;
        assert!(evidence.summary().contains("timed out after"));
        Ok(())
    }

    #[test]
    fn evaluate_rejects_a_criterion_that_is_not_automated() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let criterion = Criterion::new(
            CriterionId::new("crit-human"),
            EvaluatorKind::HumanJudgment,
            CriterionVisibility::Visible,
        );
        let task_id = TaskId::new("task-1");
        let check = spec(dir.path(), "exit 0", Duration::from_secs(5));

        let result = evaluate(&criterion, &task_id, &check, EvidenceId::new("ev-4"));
        assert!(
            matches!(result, Err(EvaluateAutomatedError::NotAutomated(_))),
            "expected a not-automated error, got: {result:?}"
        );
        let message = error_message(&result);
        assert!(message.contains("crit-human"));
        Ok(())
    }

    #[test]
    fn check_environment_reports_its_names_in_order() {
        let environment = CheckEnvironment::new()
            .with("PATH", "/usr/bin:/bin")
            .with("LANG", "C");
        assert_eq!(vec!["PATH", "LANG"], environment.names());
    }

    #[test]
    fn check_spec_working_dir_defaults_to_worktree() {
        let check = spec(
            std::path::Path::new("/tmp/worktree"),
            "exit 0",
            Duration::from_secs(1),
        );
        assert_eq!(std::path::Path::new("/tmp/worktree"), check.working_dir());
    }
}
