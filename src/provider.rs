//! The provider abstraction: the single completion call the judge makes,
//! decoupled from which model answers it.
//!
//! [`Provider`] is deliberately narrow — one method, a prompt in, raw text
//! out — because that is the entire judge contract (see `src/evaluate/judge.rs`).
//! Two implementations ship here: [`ScriptedProvider`], a queue of canned
//! responses for tests, and [`CommandProvider`], a subprocess provider that
//! makes "any model the operator can reach" a configuration fact rather than
//! an integration project.
//!
//! [`CommandProvider`] never inherits the ambient process environment
//! (RS-008): it calls [`std::process::Command::env_clear`] and applies only
//! the explicit pairs on [`CommandProviderSpec::environment`], mirroring the
//! pattern `src/git.rs` and `src/evaluate/automated.rs` already establish.
//! This module never reads `std::env` itself.

use std::io::{Read, Write as _};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

/// A stable label identifying which provider (and family) answered a
/// completion request, recorded on every verdict so a disagreement between
/// two judges can be attributed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    /// Wrap a raw provider identifier (for example `"claude"` or
    /// `"codex"`).
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

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single completion request: the prompt text and an optional model
/// override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    prompt: String,
    model: Option<String>,
}

impl CompletionRequest {
    /// Build a completion request for `prompt`, optionally naming a model.
    #[must_use]
    pub fn new(prompt: impl Into<String>, model: Option<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model,
        }
    }

    /// The prompt text.
    #[must_use]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// The requested model override, if any.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// A single completion response: the raw text a provider returned, before
/// any parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResponse {
    raw_text: String,
}

impl CompletionResponse {
    /// Wrap a provider's raw response text.
    #[must_use]
    pub fn new(raw_text: impl Into<String>) -> Self {
        Self {
            raw_text: raw_text.into(),
        }
    }

    /// The raw, unparsed response text.
    #[must_use]
    pub fn raw_text(&self) -> &str {
        &self.raw_text
    }
}

/// A failure completing a request against a provider.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProviderError {
    /// The scripted provider's canned-response queue was exhausted.
    #[error("scripted provider has no more queued responses")]
    ScriptedQueueExhausted,
    /// The subprocess could not be spawned.
    #[error("spawning `{program}`: {detail}")]
    Spawn {
        /// The program that could not be spawned.
        program: String,
        /// The OS error detail.
        detail: String,
    },
    /// Writing the prompt to the subprocess's stdin failed.
    #[error("writing prompt to `{program}` stdin: {detail}")]
    WritePrompt {
        /// The program whose stdin write failed.
        program: String,
        /// The OS error detail.
        detail: String,
    },
    /// The subprocess exited non-zero.
    #[error("`{program}` exited with status {status}: {stderr}")]
    NonZeroExit {
        /// The program that exited non-zero.
        program: String,
        /// The exit status, rendered as text (a signal termination has no
        /// numeric code).
        status: String,
        /// Captured standard error.
        stderr: String,
    },
    /// The subprocess did not finish within its configured timeout and was
    /// killed.
    #[error("`{program}` timed out after {timeout_secs}s")]
    TimedOut {
        /// The program that timed out.
        program: String,
        /// The configured timeout, in seconds.
        timeout_secs: u64,
    },
    /// The subprocess's stdout was not valid UTF-8.
    #[error("`{program}` stdout was not valid UTF-8")]
    InvalidUtf8 {
        /// The program whose stdout could not be decoded.
        program: String,
    },
}

/// One completion call, with no further contract than "text in, text out".
pub trait Provider {
    /// Complete `request`, returning the provider's raw response text.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the provider could not produce a
    /// response (a scripted queue exhausted, a subprocess failed to spawn,
    /// exited non-zero, or timed out).
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;

    /// This provider's identity, recorded on every verdict it produces.
    fn id(&self) -> &ProviderId;
}

// ---------------------------------------------------------------------
// ScriptedProvider
// ---------------------------------------------------------------------

/// A provider that returns a queued sequence of canned responses, in order.
///
/// One response is consumed per call. For tests; also public so later tasks
/// (dual-judge scenarios, orchestrator tests) can script provider behavior
/// without a subprocess.
#[derive(Debug)]
pub struct ScriptedProvider {
    id: ProviderId,
    queue: Mutex<Vec<Result<CompletionResponse, ProviderError>>>,
}

impl ScriptedProvider {
    /// Build a scripted provider that returns `responses` in order, one per
    /// [`Provider::complete`] call, then errors with
    /// [`ProviderError::ScriptedQueueExhausted`].
    #[must_use]
    pub fn new(id: ProviderId, responses: Vec<Result<CompletionResponse, ProviderError>>) -> Self {
        let mut queue = responses;
        queue.reverse();
        Self {
            id,
            queue: Mutex::new(queue),
        }
    }
}

impl Provider for ScriptedProvider {
    fn complete(&self, _request: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // A poisoned mutex means a prior call panicked mid-completion; a
        // test double has no recoverable path from that, so this recovers
        // the inner state rather than returning a misleading error.
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue
            .pop()
            .unwrap_or(Err(ProviderError::ScriptedQueueExhausted))
    }

    fn id(&self) -> &ProviderId {
        &self.id
    }
}

// ---------------------------------------------------------------------
// CommandProvider
// ---------------------------------------------------------------------

/// How the prompt reaches the subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptVia {
    /// The prompt is written to the subprocess's stdin, which is then
    /// closed.
    Stdin,
    /// The prompt is appended as the final command-line argument.
    Argument,
}

/// An explicit, ordered environment for a [`CommandProvider`]: name/value
/// pairs applied on top of [`std::process::Command::env_clear`], never the
/// ambient process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandEnvironment {
    pairs: Vec<(String, String)>,
}

impl CommandEnvironment {
    /// Add one environment variable, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.pairs.push((name.into(), value.into()));
        self
    }

    /// The configured pairs, in insertion order.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }
}

/// The full specification of a subprocess-backed provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandProviderSpec {
    /// The program to invoke.
    pub program: String,
    /// Fixed command-line arguments, before any model flag or prompt
    /// argument this spec adds.
    pub args: Vec<String>,
    /// The flag that introduces the model name (for example
    /// `Some("--model".to_owned())`), or `None` if this provider takes no
    /// model override.
    pub model_flag: Option<String>,
    /// The model name to pass after `model_flag`, if any.
    pub model: Option<String>,
    /// How the prompt reaches the subprocess.
    pub prompt_via: PromptVia,
    /// The subprocess's explicit environment. Never the ambient process
    /// environment (RS-008).
    pub environment: CommandEnvironment,
    /// How long to wait for the subprocess before killing it.
    pub timeout: Duration,
}

/// How often [`CommandProvider::complete`] polls the child process while
/// waiting for it to finish or for its timeout to expire.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A provider that runs an operator-configured subprocess for each call.
///
/// `claude -p --model <m>`, `codex exec`, `opencode run`, or any other
/// command that reads a prompt and writes a response. This is what makes
/// the judge "any model the operator can reach" a configuration axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandProvider {
    id: ProviderId,
    spec: CommandProviderSpec,
}

impl CommandProvider {
    /// Build a command provider identified by `id`, invoking `spec`.
    #[must_use]
    pub const fn new(id: ProviderId, spec: CommandProviderSpec) -> Self {
        Self { id, spec }
    }

    fn build_command(&self, request: &CompletionRequest) -> Command {
        let mut command = Command::new(&self.spec.program);
        command.env_clear();
        for (name, value) in self.spec.environment.pairs() {
            command.env(name, value);
        }
        command.args(&self.spec.args);
        if let Some(flag) = &self.spec.model_flag {
            let model = request.model().or(self.spec.model.as_deref());
            if let Some(model) = model {
                command.arg(flag);
                command.arg(model);
            }
        }
        if self.spec.prompt_via == PromptVia::Argument {
            command.arg(request.prompt());
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Lead a process group of its own, so a timeout can kill every
            // descendant (a pipeline member, a helper process a provider CLI
            // starts) rather than only the immediate child, which would leave
            // descendants holding the stdout/stderr pipes open and the call
            // waiting on them. Mirrors `run_check` in `src/evaluate/automated.rs`.
            command.process_group(0);
        }
        command
    }
}

/// Send `SIGKILL` to the whole process group led by `pid`, the provider's
/// immediate child (see `process_group(0)` in [`CommandProvider::build_command`]).
///
/// This runs the POSIX shell builtin `kill` through `/bin/sh` rather than
/// linking `libc` for one syscall, which the crate's `unsafe_code` denial rules
/// out, with an empty environment, as `src/evaluate/automated.rs` does for
/// checks. Best-effort: if it cannot even spawn, the `child.kill()` at the call
/// site still reaches the immediate child.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -KILL -{pid}"))
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Write `prompt` to `stdin`, then close it (signaling EOF to the child).
///
/// `stdin` is `None` only if a caller other than [`CommandProvider`] reused
/// this helper without piping stdin; [`CommandProvider::complete`] always
/// pipes it, so that case is a silent no-op rather than a fabricated error.
///
/// Runs on its own thread in [`CommandProvider::complete`] (never on the
/// thread running the poll/timeout loop): a prompt larger than the pipe
/// buffer, written to a child that produces output before it finishes
/// reading stdin, would otherwise block this call inside `write_all`
/// forever — a syscall the timeout loop never reaches. Moving the write to
/// a dedicated thread means the poll loop always runs and a stuck write is
/// unblocked (as a broken pipe) the moment the child is killed on timeout.
fn write_prompt_via_stdin(
    stdin: Option<ChildStdin>,
    prompt: &str,
    program: &str,
) -> Result<(), ProviderError> {
    let Some(mut stdin) = stdin else {
        return Ok(());
    };
    stdin
        .write_all(prompt.as_bytes())
        .map_err(|source| ProviderError::WritePrompt {
            program: program.to_owned(),
            detail: source.to_string(),
        })
}

/// Read `stream` to end into a byte buffer, on its own thread. Runs
/// concurrently with the prompt writer and the poll/timeout loop so a
/// child that writes substantial output before it has finished reading
/// stdin cannot deadlock against an undrained pipe either. A read error is
/// silently treated as "nothing more to read" (best-effort, matching
/// `src/evaluate/automated.rs`'s treatment of a killed child's partial
/// output).
fn spawn_reader<R>(stream: Option<R>) -> JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    })
}

/// Join a reader thread, treating a panic the same as "nothing read" —
/// there is no partial-output value to recover from a panicked reader
/// thread, and a killed child's pipes closing early already covers the
/// "less output than expected" case without one.
fn join_reader(handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Join the prompt-writer thread, treating a panic as a
/// [`ProviderError::WritePrompt`] rather than propagating it.
fn join_writer(
    handle: JoinHandle<Result<(), ProviderError>>,
    program: &str,
) -> Result<(), ProviderError> {
    handle.join().unwrap_or_else(|_panic| {
        Err(ProviderError::WritePrompt {
            program: program.to_owned(),
            detail: "prompt-writer thread panicked".to_owned(),
        })
    })
}

impl Provider for CommandProvider {
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let program = self.spec.program.clone();
        let mut command = self.build_command(request);

        let mut child = command.spawn().map_err(|source| ProviderError::Spawn {
            program: program.clone(),
            detail: source.to_string(),
        })?;

        // Drain stdout/stderr on their own threads immediately, before the
        // poll loop starts: a child that writes enough output before it
        // finishes reading stdin must never be left with a full pipe and
        // nothing reading the other end.
        let stdout_handle = spawn_reader(child.stdout.take());
        let stderr_handle = spawn_reader(child.stderr.take());

        // `build_command` always configures `Stdio::piped()` for stdin, so
        // `child.stdin` is structurally always `Some` here; there is no
        // "stdin was not piped" case to report as a typed error.
        let writer_handle = if self.spec.prompt_via == PromptVia::Stdin {
            let stdin = child.stdin.take();
            let prompt = request.prompt().to_owned();
            let writer_program = program.clone();
            Some(thread::spawn(move || {
                write_prompt_via_stdin(stdin, &prompt, &writer_program)
            }))
        } else {
            // Nothing is written to stdin, but it must still be closed so a
            // child that reads stdin unconditionally does not block forever.
            drop(child.stdin.take());
            None
        };

        let status_result = self.wait_for_exit(&mut child, &program);

        let stdout_buf = join_reader(stdout_handle);
        let stderr_buf = join_reader(stderr_handle);
        let writer_result = writer_handle.map(|handle| join_writer(handle, &program));

        Self::resolve_outcome(
            &program,
            status_result,
            writer_result,
            stdout_buf,
            &stderr_buf,
        )
    }

    fn id(&self) -> &ProviderId {
        &self.id
    }
}

impl CommandProvider {
    /// Poll `child` until it exits or `self.spec.timeout` elapses.
    ///
    /// A `try_wait` failure is treated the same as "still running": both
    /// fall through to another poll, so an eventual [`ProviderError::TimedOut`]
    /// is the only outcome an unreadable wait status can produce, rather
    /// than a second, practically unreachable error variant. This never
    /// touches stdout/stderr/stdin itself — those are drained or written on
    /// their own threads (see [`Provider::complete`]) so this loop is
    /// always free to notice the timeout regardless of what the child's
    /// pipes are doing.
    fn wait_for_exit(&self, child: &mut Child, program: &str) -> Result<ExitStatus, ProviderError> {
        let deadline = Instant::now() + self.spec.timeout;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                #[cfg(unix)]
                kill_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProviderError::TimedOut {
                    program: program.to_owned(),
                    timeout_secs: self.spec.timeout.as_secs(),
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Combine the poll loop's outcome with the joined reader/writer
    /// threads' results into the final [`CompletionResponse`] or
    /// [`ProviderError`].
    ///
    /// A timeout wins outright (the writer's broken-pipe error from being
    /// killed is not informative once the process has already been judged
    /// too slow). Otherwise a non-zero exit wins over a writer error too:
    /// the process's own exit code is the more specific diagnosis of what
    /// went wrong. Only once the process exited zero does a writer error
    /// (the prompt was not fully delivered) get to fail the call.
    fn resolve_outcome(
        program: &str,
        status_result: Result<ExitStatus, ProviderError>,
        writer_result: Option<Result<(), ProviderError>>,
        stdout_buf: Vec<u8>,
        stderr_buf: &[u8],
    ) -> Result<CompletionResponse, ProviderError> {
        let status = status_result?;

        if !status.success() {
            return Err(ProviderError::NonZeroExit {
                program: program.to_owned(),
                status: status.to_string(),
                stderr: String::from_utf8_lossy(stderr_buf).into_owned(),
            });
        }

        if let Some(Err(write_err)) = writer_result {
            return Err(write_err);
        }

        String::from_utf8(stdout_buf)
            .map(CompletionResponse::new)
            .map_err(|_source| ProviderError::InvalidUtf8 {
                program: program.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandEnvironment, CommandProvider, CommandProviderSpec, CompletionRequest,
        CompletionResponse, PromptVia, Provider, ProviderError, ProviderId, ScriptedProvider,
    };
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn provider_id_displays_and_exposes_raw_value() {
        let id = ProviderId::new("claude");
        assert_eq!("claude", id.as_str());
        assert_eq!("claude", id.to_string());
    }

    #[test]
    fn completion_request_exposes_prompt_and_model() {
        let request = CompletionRequest::new("hello", Some("m1".to_owned()));
        assert_eq!("hello", request.prompt());
        assert_eq!(Some("m1"), request.model());

        let no_model = CompletionRequest::new("hello", None);
        assert_eq!(None, no_model.model());
    }

    #[test]
    fn completion_response_exposes_raw_text() {
        let response = CompletionResponse::new("raw");
        assert_eq!("raw", response.raw_text());
    }

    #[test]
    fn scripted_provider_returns_responses_in_order() {
        let provider = ScriptedProvider::new(
            ProviderId::new("scripted"),
            vec![
                Ok(CompletionResponse::new("first")),
                Ok(CompletionResponse::new("second")),
            ],
        );
        let request = CompletionRequest::new("prompt", None);
        assert_eq!(
            "first",
            provider
                .complete(&request)
                .map(|r| r.raw_text().to_owned())
                .unwrap_or_default()
        );
        assert_eq!(
            "second",
            provider
                .complete(&request)
                .map(|r| r.raw_text().to_owned())
                .unwrap_or_default()
        );
        assert_eq!(
            Err(ProviderError::ScriptedQueueExhausted),
            provider.complete(&request)
        );
    }

    #[test]
    fn scripted_provider_can_queue_errors() {
        let provider = ScriptedProvider::new(
            ProviderId::new("scripted"),
            vec![Err(ProviderError::ScriptedQueueExhausted)],
        );
        let request = CompletionRequest::new("prompt", None);
        assert_eq!(
            Err(ProviderError::ScriptedQueueExhausted),
            provider.complete(&request)
        );
    }

    #[test]
    fn scripted_provider_reports_its_id() {
        let provider = ScriptedProvider::new(ProviderId::new("scripted-a"), vec![]);
        assert_eq!("scripted-a", provider.id().as_str());
    }

    #[test]
    fn command_environment_builds_pairs() {
        let env = CommandEnvironment::default()
            .with("PATH", "/usr/bin")
            .with("HOME", "/home/x");
        assert_eq!(
            &[
                ("PATH".to_owned(), "/usr/bin".to_owned()),
                ("HOME".to_owned(), "/home/x".to_owned())
            ],
            env.pairs()
        );
    }

    fn cat_spec(prompt_via: PromptVia) -> CommandProviderSpec {
        CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "cat".to_owned()],
            model_flag: None,
            model: None,
            prompt_via,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn command_provider_echoes_prompt_via_stdin() -> Result<(), Box<dyn std::error::Error>> {
        let provider = CommandProvider::new(ProviderId::new("sh-cat"), cat_spec(PromptVia::Stdin));
        let request = CompletionRequest::new("hello from stdin", None);
        let response = provider.complete(&request)?;
        assert_eq!("hello from stdin", response.raw_text());
        Ok(())
    }

    #[test]
    fn command_provider_reports_nonzero_exit() {
        let spec = CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "echo bad >&2; exit 3".to_owned()],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_secs(5),
        };
        let provider = CommandProvider::new(ProviderId::new("sh-fail"), spec);
        let request = CompletionRequest::new("irrelevant", None);
        let result = provider.complete(&request);
        assert!(matches!(
            &result,
            Err(ProviderError::NonZeroExit { stderr, .. }) if stderr.contains("bad")
        ));
    }

    #[test]
    fn command_provider_reports_timeout() {
        let spec = CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 5".to_owned()],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_millis(50),
        };
        let provider = CommandProvider::new(ProviderId::new("sh-slow"), spec);
        let request = CompletionRequest::new("irrelevant", None);
        assert_eq!(
            Err(ProviderError::TimedOut {
                program: "sh".to_owned(),
                timeout_secs: 0,
            }),
            provider.complete(&request)
        );
    }

    #[test]
    fn command_provider_round_trips_a_multi_megabyte_prompt_via_stdin()
    -> Result<(), Box<dyn std::error::Error>> {
        // A prompt well past any OS pipe buffer (typically 64KiB): if the
        // write to stdin were not on its own thread, this would block
        // inside `write_all` forever, since `cat` only starts writing to
        // stdout once it has read all of stdin, and stdout is drained
        // concurrently rather than only after the child exits.
        let big_prompt = "x".repeat(8 * 1024 * 1024);
        let provider =
            CommandProvider::new(ProviderId::new("sh-cat-big"), cat_spec(PromptVia::Stdin));
        let request = CompletionRequest::new(big_prompt.clone(), None);
        let response = provider.complete(&request)?;
        assert_eq!(big_prompt, response.raw_text());
        Ok(())
    }

    #[test]
    fn command_provider_times_out_promptly_with_a_large_prompt_and_a_non_reading_child() {
        // The child never reads stdin at all (`sleep`), so without the
        // writer thread this call would block inside `write_all` well past
        // the configured timeout, or forever if the prompt exceeds the
        // pipe buffer. It must report `TimedOut` close to the configured
        // timeout, not after `sleep`'s own duration.
        let big_prompt = "y".repeat(8 * 1024 * 1024);
        let spec = CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_millis(200),
        };
        let provider = CommandProvider::new(ProviderId::new("sh-sleep-big"), spec);
        let request = CompletionRequest::new(big_prompt, None);

        let started = Instant::now();
        let result = provider.complete(&request);
        let elapsed = started.elapsed();

        assert!(matches!(
            result,
            Err(ProviderError::TimedOut {
                timeout_secs: 0,
                ..
            })
        ));
        assert!(
            elapsed < Duration::from_secs(10),
            "expected a prompt timeout, not a wait for `sleep 30`; took {elapsed:?}"
        );
    }

    #[test]
    fn command_provider_timeout_kills_a_pipeline_descendant() {
        // `sleep` in a pipeline is a genuine descendant of `sh` under every
        // shell, unlike a lone `sleep`, which a shell may exec in place. It
        // holds the stdout pipe open, so a timeout that kills only the
        // immediate child waits for `sleep` to finish on its own. The marker
        // carries this test process's pid so the absence check below cannot
        // match a concurrent run of the same test, and the duration is short
        // enough that an unfixed run fails instead of hanging.
        let marker = format!("sleep 29.{}", std::process::id());
        let spec = CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), format!("{marker} | cat")],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_millis(200),
        };
        let provider = CommandProvider::new(ProviderId::new("sh-pipeline"), spec);
        let request = CompletionRequest::new("irrelevant", None);

        let started = Instant::now();
        let result = provider.complete(&request);
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(ProviderError::TimedOut { .. })));
        assert!(
            elapsed < Duration::from_secs(10),
            "expected the timeout to kill the pipeline promptly; took {elapsed:?}"
        );
        let survived = Command::new("pgrep")
            .arg("-f")
            .arg(&marker)
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            !survived,
            "expected no `{marker}` descendant to survive the timeout"
        );
    }

    #[test]
    fn command_provider_reports_broken_pipe_writing_the_prompt() {
        // The child exits immediately without reading stdin; by the time
        // the large prompt is written, the read end is closed and the
        // write fails with a broken pipe rather than blocking.
        let spec = CommandProviderSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "exit 0".to_owned()],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
            timeout: Duration::from_secs(5),
        };
        let provider = CommandProvider::new(ProviderId::new("sh-exit-fast"), spec);
        let big_prompt = "x".repeat(16 * 1024 * 1024);
        let request = CompletionRequest::new(big_prompt, None);
        let result = provider.complete(&request);
        assert!(matches!(result, Err(ProviderError::WritePrompt { .. })));
    }

    #[test]
    fn write_prompt_via_stdin_is_a_no_op_when_stdin_is_absent() {
        assert_eq!(
            Ok(()),
            super::write_prompt_via_stdin(None, "prompt", "program")
        );
    }

    #[test]
    fn join_writer_reports_a_panicked_thread_as_write_prompt_error() {
        let previous_hook = std::panic::take_hook();
        // The default hook prints the panic to stderr; this test's panic is
        // deliberate, so suppress that noise for the duration of the call.
        std::panic::set_hook(Box::new(|_info| {}));
        let handle = thread::spawn(|| -> Result<(), ProviderError> {
            unreachable!("deliberate panic for join_writer's panic-handling path")
        });
        let result = super::join_writer(handle, "program");
        std::panic::set_hook(previous_hook);

        assert!(matches!(
            result,
            Err(ProviderError::WritePrompt { program, detail })
                if program == "program" && detail.contains("panicked")
        ));
    }

    #[test]
    fn command_provider_reports_its_id() {
        let provider = CommandProvider::new(ProviderId::new("sh-cat"), cat_spec(PromptVia::Stdin));
        assert_eq!("sh-cat", provider.id().as_str());
    }

    #[test]
    fn command_provider_reports_spawn_failure() {
        let spec = CommandProviderSpec {
            program: "definitely-not-a-real-binary-xyz".to_owned(),
            args: vec![],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default(),
            timeout: Duration::from_secs(1),
        };
        let provider = CommandProvider::new(ProviderId::new("missing"), spec);
        let request = CompletionRequest::new("irrelevant", None);
        let result = provider.complete(&request);
        assert!(matches!(
            &result,
            Err(ProviderError::Spawn { program, .. }) if program == "definitely-not-a-real-binary-xyz"
        ));
    }

    #[test]
    fn command_provider_passes_prompt_as_argument() -> Result<(), Box<dyn std::error::Error>> {
        let provider = CommandProvider::new(
            ProviderId::new("sh-echo"),
            CommandProviderSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "printf %s \"$0\"".to_owned()],
                model_flag: None,
                model: None,
                prompt_via: PromptVia::Argument,
                environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
                timeout: Duration::from_secs(5),
            },
        );
        let request = CompletionRequest::new("hi-there", None);
        let response = provider.complete(&request)?;
        assert_eq!("hi-there", response.raw_text());
        Ok(())
    }

    #[test]
    fn command_provider_applies_model_flag() -> Result<(), Box<dyn std::error::Error>> {
        let provider = CommandProvider::new(
            ProviderId::new("sh-model"),
            CommandProviderSpec {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    // Drain stdin first: a child that exits 0 without reading the
                    // prompt races the writer thread, and a lost race is a broken
                    // pipe that `resolve_outcome` correctly reports as an error.
                    "cat >/dev/null; printf %s \"$2\"".to_owned(),
                    "_".to_owned(),
                ],
                model_flag: Some("--model".to_owned()),
                model: Some("fallback-model".to_owned()),
                prompt_via: PromptVia::Stdin,
                environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
                timeout: Duration::from_secs(5),
            },
        );
        let request = CompletionRequest::new("prompt-text", Some("request-model".to_owned()));
        let response = provider.complete(&request)?;
        assert_eq!("request-model", response.raw_text());
        Ok(())
    }

    #[test]
    fn command_provider_reports_invalid_utf8_stdout() {
        let provider = CommandProvider::new(
            ProviderId::new("sh-binary"),
            CommandProviderSpec {
                program: "sh".to_owned(),
                // Drain stdin first, for the same reason as
                // `command_provider_applies_model_flag`. The bytes are written
                // with octal escapes: POSIX printf defines `\ddd` but not `\x`,
                // and dash, `/bin/sh` on the Linux runners, prints `\xff` literally.
                args: vec![
                    "-c".to_owned(),
                    "cat >/dev/null; printf '\\377\\376'".to_owned(),
                ],
                model_flag: None,
                model: None,
                prompt_via: PromptVia::Stdin,
                environment: CommandEnvironment::default().with("PATH", "/usr/bin:/bin"),
                timeout: Duration::from_secs(5),
            },
        );
        let request = CompletionRequest::new("irrelevant", None);
        let result = provider.complete(&request);
        assert!(matches!(
            &result,
            Err(ProviderError::InvalidUtf8 { program }) if program == "sh"
        ));
    }

    #[test]
    fn provider_error_variants_display() {
        assert!(
            ProviderError::ScriptedQueueExhausted
                .to_string()
                .contains("queued")
        );
        assert!(
            ProviderError::Spawn {
                program: "p".to_owned(),
                detail: "d".to_owned()
            }
            .to_string()
            .contains('p')
        );
        assert!(
            ProviderError::WritePrompt {
                program: "p".to_owned(),
                detail: "d".to_owned()
            }
            .to_string()
            .contains('p')
        );
        assert!(
            ProviderError::NonZeroExit {
                program: "p".to_owned(),
                status: "1".to_owned(),
                stderr: "e".to_owned()
            }
            .to_string()
            .contains('p')
        );
        assert!(
            ProviderError::TimedOut {
                program: "p".to_owned(),
                timeout_secs: 5
            }
            .to_string()
            .contains('p')
        );
        assert!(
            ProviderError::InvalidUtf8 {
                program: "p".to_owned()
            }
            .to_string()
            .contains('p')
        );
    }
}
