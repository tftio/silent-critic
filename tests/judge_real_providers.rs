//! Manual, `#[ignore]`d acceptance check for T008 item 4: run the judge once
//! against real providers of different families (Anthropic's `claude -p`,
//! `OpenAI`'s `codex exec`, and opencode's own gateway via `opencode run`)
//! on one real diff, printing both raw responses and parsed verdicts. This
//! is completion evidence, not a CI test -- it requires the relevant CLIs
//! on `PATH` and authenticated for the operator, and it makes a real
//! network call to each provider's API.
//!
//! Run with:
//!
//! ```text
//! cargo test --test judge_real_providers -- --ignored --nocapture
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use silent_critic::evaluate::judge::{
    DirectoryRawResponseSink, JudgeCriterion, JudgeError, JudgeInput, RetryBudget, judge_once,
};
use silent_critic::git::{self, GitFacts, GitInvocation};
use silent_critic::model::{BaseRef, CriterionId, EvaluatorKind, NonEmptyString, TaskId};
use silent_critic::provider::{
    CommandEnvironment, CommandProvider, CommandProviderSpec, PromptVia, Provider, ProviderError,
    ProviderId,
};

#[allow(
    clippy::disallowed_methods,
    reason = "test/binary edge: reading the operator's real PATH/HOME so the provider CLIs can find their own config and auth, not read by the library under test"
)]
fn ambient_env(pairs: &[&str]) -> CommandEnvironment {
    let mut env = CommandEnvironment::default();
    for name in pairs {
        if let Ok(value) = std::env::var(name) {
            env = env.with((*name).to_owned(), value);
        }
    }
    env
}

/// Reading `PATH` here is test code finding the real `git` binary, not the
/// library reading its own environment.
#[allow(clippy::disallowed_methods)]
fn test_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Run `git` against `repo` with a hermetic environment: `env_clear()` plus
/// an explicit, minimal environment, so this test's own fixture repository
/// is never affected by (nor operates against) an ambient
/// `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` -- git exports all three into
/// any process it spawns as a hook (this crate's own pre-commit hook runs
/// `cargo nextest`), and a test spawning `git` without clearing them
/// operates against the outer repository's index/worktree instead of its
/// own, corrupting it.
fn git_cmd(repo: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(repo)
        .env_clear()
        .env("PATH", test_path())
        .env("HOME", repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()?;
    if !status.success() {
        return Err(format!("git {args:?} failed with {status}").into());
    }
    Ok(())
}

/// Build the shared fixture: a temporary repo with one small, judgeable
/// diff (a renamed behavior plus a doc comment describing it).
fn build_fixture() -> Result<(tempfile::TempDir, GitFacts), Box<dyn Error>> {
    let dir = tempfile::TempDir::new()?;
    let repo = dir.path().to_path_buf();
    git_cmd(&repo, &["-c", "init.defaultBranch=main", "init", "-q"])?;
    std::fs::write(
        repo.join("greet.rs"),
        "fn greet() -> &'static str {\n    \"hi\"\n}\n",
    )?;
    git_cmd(&repo, &["add", "-A"])?;
    git_cmd(&repo, &["commit", "-q", "-m", "base"])?;

    std::fs::write(
        repo.join("greet.rs"),
        "/// Returns a friendly, informal greeting.\nfn greet() -> &'static str {\n    \"hello, world\"\n}\n",
    )?;
    git_cmd(&repo, &["add", "-A"])?;
    git_cmd(&repo, &["commit", "-q", "-m", "head"])?;

    let invocation = GitInvocation::new(
        "git".to_owned(),
        test_path(),
        repo.to_string_lossy().into_owned(),
    );
    let facts = git::capture(&repo, &BaseRef::new("HEAD~1"), &invocation)?;
    Ok((dir, facts))
}

fn fixture_criteria() -> Result<Vec<JudgeCriterion>, Box<dyn Error>> {
    Ok(vec![
        JudgeCriterion::new(
            CriterionId::new("returns-friendly-string"),
            NonEmptyString::new("greet() returns a friendly greeting string")?,
            EvaluatorKind::AgentEvaluated,
            Some(NonEmptyString::new(
                "Does the diff make greet() return a friendly greeting?",
            )?),
            None,
        ),
        JudgeCriterion::new(
            CriterionId::new("doc-comment-matches-behavior"),
            NonEmptyString::new(
                "any new doc comment on greet() accurately describes its behavior",
            )?,
            EvaluatorKind::AgentEvaluated,
            Some(NonEmptyString::new(
                "If the diff adds a doc comment on greet(), does it accurately describe what the function does?",
            )?),
            None,
        ),
    ])
}

/// Whether `source` indicates the provider itself could not be reached at
/// all (missing binary) or refused the request (typically an
/// authentication failure surfacing as a non-zero exit) -- the two cases
/// this manual check tolerates by skipping rather than failing, since they
/// say nothing about `judge_once`'s own correctness. Any other
/// `ProviderError` (a broken prompt write, a timeout, invalid UTF-8 output)
/// is a real plumbing problem and must fail the test.
const fn is_provider_unavailable(source: &ProviderError) -> bool {
    matches!(
        source,
        ProviderError::Spawn { .. } | ProviderError::NonZeroExit { .. }
    )
}

/// Run the judge once against `provider`, printing the outcome (or error)
/// under `label` for the report transcript.
///
/// A well-formed verdict is asserted, not merely printed: this is the
/// substance of the acceptance check, that a real provider round-trips a
/// real diff through `judge_once` into a well-formed `JudgeOutcome`. A
/// provider that is unreachable or unauthenticated in this environment is
/// skipped (with the exact error printed) rather than failing the test;
/// any other failure (a malformed response despite reaching the provider,
/// a broken prompt write, a timeout) is a genuine bug and fails it.
fn run_and_report(
    label: &str,
    input: &JudgeInput<'_>,
    provider: &dyn Provider,
    sink: &DirectoryRawResponseSink,
) -> Result<(), Box<dyn Error>> {
    eprintln!("=== {label} ===");
    match judge_once(input, provider, sink, RetryBudget::default()) {
        Ok(outcome) => {
            eprintln!("PARSED VERDICT ({label}): {outcome:#?}");
            assert!(
                !outcome.verdicts.is_empty(),
                "{label}: judge produced a verdict with no per-criterion judgments"
            );
            Ok(())
        }
        Err(JudgeError::Provider { source, .. }) if is_provider_unavailable(&source) => {
            eprintln!("SKIPPING {label}: provider unavailable or unauthenticated: {source}");
            Ok(())
        }
        Err(err) => {
            eprintln!("JUDGE ERROR ({label}): {err}");
            Err(format!(
                "{label}: unexpected judge error (not an unavailability/auth failure): {err}"
            )
            .into())
        }
    }
}

#[test]
#[ignore = "manual completion-evidence check: requires the provider CLIs on PATH and authenticated"]
fn judge_runs_against_two_real_provider_families() -> Result<(), Box<dyn Error>> {
    let (_dir, facts) = build_fixture()?;
    let criteria = fixture_criteria()?;
    let task_id = TaskId::new("T-real-provider-check");
    let checks = Vec::new();
    let input = JudgeInput::new(&task_id, &criteria, &facts, &checks);

    // Each provider gets its own subdirectory: `DirectoryRawResponseSink`
    // names files `judge-<task>-<attempt>.txt`, and this test runs every
    // provider against the same task id, so a shared sink would let a
    // later provider's attempt 1 overwrite an earlier one's.
    let raw_dir = PathBuf::from("/tmp/silent-critic-judge-raw");
    let claude_dir = raw_dir.join("claude");
    let codex_dir = raw_dir.join("codex");
    let opencode_dir = raw_dir.join("opencode");
    std::fs::create_dir_all(&claude_dir)?;
    std::fs::create_dir_all(&codex_dir)?;
    std::fs::create_dir_all(&opencode_dir)?;
    let claude_sink = DirectoryRawResponseSink::new(&claude_dir);
    let codex_sink = DirectoryRawResponseSink::new(&codex_dir);
    let opencode_sink = DirectoryRawResponseSink::new(&opencode_dir);

    let claude_provider = CommandProvider::new(
        ProviderId::new("claude"),
        CommandProviderSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned()],
            model_flag: Some("--model".to_owned()),
            model: Some("claude-haiku-4-5".to_owned()),
            prompt_via: PromptVia::Stdin,
            environment: ambient_env(&["PATH", "HOME", "TMPDIR", "USER"]),
            timeout: Duration::from_mins(2),
        },
    );
    run_and_report(
        "claude -p --model claude-haiku-4-5",
        &input,
        &claude_provider,
        &claude_sink,
    )?;

    let codex_provider = CommandProvider::new(
        ProviderId::new("codex"),
        CommandProviderSpec {
            program: "codex".to_owned(),
            args: vec!["exec".to_owned()],
            model_flag: Some("--model".to_owned()),
            model: Some("gpt-5-mini".to_owned()),
            prompt_via: PromptVia::Argument,
            environment: ambient_env(&["PATH", "HOME", "TMPDIR", "USER", "OPENAI_API_KEY"]),
            timeout: Duration::from_mins(3),
        },
    );
    run_and_report(
        "codex exec --model gpt-5-mini",
        &input,
        &codex_provider,
        &codex_sink,
    )?;

    // A second, non-Anthropic family, since `codex` is unauthenticated in
    // this environment: opencode's own free-tier gateway. `opencode run`
    // prints only the model's answer to stdout (its banner/ANSI framing go
    // to stderr), which is exactly what `CommandProvider` needs.
    let opencode_provider = CommandProvider::new(
        ProviderId::new("opencode"),
        CommandProviderSpec {
            program: "opencode".to_owned(),
            args: vec!["run".to_owned()],
            model_flag: Some("--model".to_owned()),
            model: Some("opencode/nemotron-3-ultra-free".to_owned()),
            prompt_via: PromptVia::Argument,
            environment: ambient_env(&["PATH", "HOME", "TMPDIR", "USER"]),
            timeout: Duration::from_mins(4),
        },
    );
    run_and_report(
        "opencode run --model opencode/nemotron-3-ultra-free",
        &input,
        &opencode_provider,
        &opencode_sink,
    )?;

    eprintln!("raw responses persisted under: {}", raw_dir.display());
    Ok(())
}
