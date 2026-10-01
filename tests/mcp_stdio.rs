//! `silent-critic-mcp` over real stdio (T006).
//!
//! The tool schema on the wire is API an agent builds prompts against, so it
//! is covered by speaking the actual protocol to the actual binary rather
//! than by calling `silent_critic::tools::Tools` in process (T005's `tests/tools.rs`
//! already covers every semantic there). These tests assert only what the
//! wire carries: that the tool set and schemas match
//! [`silent_critic::tools::ToolSurface::specs`] exactly, and that a call's result
//! is the same text [`silent_critic::tools::ToolSurface::call`] would give for the
//! same arguments — never that the tools behave correctly, which is T005's
//! job.
//!
//! Framing is newline-delimited JSON-RPC, which is what MCP's stdio
//! transport specifies; each request is one line and each response is one
//! line.

use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

use silent_critic::model::{PlanId, TaskId};
use silent_critic::store::{Store, StoreRoot, default_store_root};
use silent_critic::token::{Role, Token, TokenRegistry};
use silent_critic::tools::{ToolOutcome, ToolSurface, Tools};

type TestResult = Result<(), Box<dyn Error>>;

const STORE_PLAN: &str = include_str!("fixtures/2026-09-05-store-plan.md");
const SENTINEL_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");

/// Reading `PATH` here is test code finding the real `git` binary, not the
/// library reading its own environment.
#[allow(clippy::disallowed_methods)]
fn test_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Run `git` against `dir` with a hermetic environment: `env_clear()` plus
/// an explicit, minimal environment. `env_remove("GIT_DIR")`/
/// `env_remove("GIT_WORK_TREE")` alone is not enough -- git also exports
/// `GIT_INDEX_FILE` into any process it spawns as a hook (this crate's own
/// pre-commit hook runs `cargo nextest`), and a test spawning `git` for its
/// own temporary repository without clearing the full environment then
/// operates against the outer repository's index/worktree instead of its
/// own, corrupting it.
fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("PATH", test_path())
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {args:?} failed in {}", dir.display()).into())
    }
}

fn init_repo(dir: &Path) -> Result<(), Box<dyn Error>> {
    git(dir, &["init", "--quiet"])?;
    std::fs::write(dir.join("README.md"), "hello\n")?;
    git(dir, &["add", "README.md"])?;
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial commit",
        ],
    )
}

/// A repository and store with `plan_body` added under it.
struct Fixture {
    _repo_dir: tempfile::TempDir,
    store_dir: tempfile::TempDir,
    store: Store,
    repo_start: PathBuf,
    plan_id: String,
}

fn set_up(plan_body: &str) -> Result<Fixture, Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-fixture-plan.md");
    std::fs::write(&plan_path, plan_body)?;

    let store_dir = tempfile::tempdir()?;
    let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
    let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

    Ok(Fixture {
        repo_start: repo_dir.path().to_path_buf(),
        plan_id: plan_id.as_str().to_owned(),
        store,
        _repo_dir: repo_dir,
        store_dir,
    })
}

fn orchestrator_tools(fixture: &Fixture) -> Tools<'_> {
    Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
}

/// Mint a worker token for `task_id` and persist the registry to the
/// fixture's own `<plan_dir>/run/tokens.toml`, exactly where
/// `silent-critic-mcp`'s process edge reads it from a presented `SILENT_CRITIC_TOKEN`.
fn mint_worker_token(fixture: &Fixture, task_id: &str) -> Result<(Token, TaskId), Box<dyn Error>> {
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task = TaskId::new(task_id);
    let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;
    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let plan_dir = plan_path.parent().ok_or("plan path has no parent")?;
    registry.save(&plan_dir.join("run").join("tokens.toml"))?;
    Ok((token, task))
}

/// A running `silent-critic-mcp` process, with the session already initialized.
struct Session {
    child: Child,
    /// `None` once the session has been shut down. Closing stdin is how the
    /// server is told to stop: killing it instead leaves it no chance to
    /// flush its coverage profile, which is why these tests would otherwise
    /// report `main` as unexecuted.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Session {
    fn start(fixture: &Fixture, token: Option<&str>) -> Result<Self, Box<dyn Error>> {
        Self::start_with(fixture, token, true, None, None)
    }

    /// [`Session::start`], but without `SILENT_CRITIC_REPO` set: the process edge
    /// must fall back to its own current directory (`std::env::current_dir`
    /// in `src/bin/silent-critic-mcp.rs`), so the child is spawned with the
    /// fixture's repository as its working directory instead.
    fn start_with_repo_defaulted_to_cwd(fixture: &Fixture) -> Result<Self, Box<dyn Error>> {
        Self::start_with(fixture, None, false, None, None)
    }

    /// [`Session::start`] for a worker session, but with `SILENT_CRITIC_TASK_ID`
    /// also set (fix round 2, finding #8): the process edge should pass
    /// `task_id` as `Tools::worker`'s caller-declared `expected_task`
    /// rather than deriving it from `token`'s own resolution.
    fn start_worker_with_task_id(
        fixture: &Fixture,
        token: &str,
        task_id: &str,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with(fixture, Some(token), true, Some(task_id), None)
    }

    /// [`Session::start`] for an orchestrator session, with `SILENT_CRITIC_CONFIG`
    /// also set (fix round 2, finding #2): the process edge should load
    /// `config_path` and wire its `[harness]`/`[judge]` sections through to
    /// `Tools::orchestrator`/`Tools::with_judge`.
    fn start_orchestrator_with_config(
        fixture: &Fixture,
        config_path: &Path,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with(fixture, None, true, None, Some(config_path))
    }

    fn start_with(
        fixture: &Fixture,
        token: Option<&str>,
        set_repo_env: bool,
        task_id: Option<&str>,
        config_path: Option<&Path>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_silent-critic-mcp"));
        command
            .env("SILENT_CRITIC_STORE_ROOT", fixture.store_dir.path())
            .env("SILENT_CRITIC_PLAN_ID", &fixture.plan_id)
            .env_remove("XDG_DATA_HOME")
            .env_remove("HOME");
        if set_repo_env {
            command.env("SILENT_CRITIC_REPO", &fixture.repo_start);
        } else {
            command
                .env_remove("SILENT_CRITIC_REPO")
                .current_dir(&fixture.repo_start);
        }
        match token {
            Some(token) => {
                command.env("SILENT_CRITIC_TOKEN", token);
            }
            None => {
                command.env_remove("SILENT_CRITIC_TOKEN");
            }
        }
        match task_id {
            Some(task_id) => {
                command.env("SILENT_CRITIC_TASK_ID", task_id);
            }
            None => {
                command.env_remove("SILENT_CRITIC_TASK_ID");
            }
        }
        match config_path {
            Some(config_path) => {
                command.env("SILENT_CRITIC_CONFIG", config_path);
            }
            None => {
                command.env_remove("SILENT_CRITIC_CONFIG");
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("no stdin on the server")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout on the server")?);
        let mut session = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
        };
        session.request(
            "initialize",
            &json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "silent-critic-tests", "version": "0" },
            }),
        )?;
        session.notify("notifications/initialized", &json!({}))?;
        Ok(session)
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.stdin.as_mut().ok_or("the session is shut down")?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                return Err(format!("the server closed while waiting for {method}").into());
            }
            let parsed: Value = serde_json::from_str(line.trim())?;
            if parsed.get("id").and_then(Value::as_i64) == Some(id) {
                return Ok(parsed);
            }
        }
    }

    fn notify(&mut self, method: &str, params: &Value) -> Result<(), Box<dyn Error>> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let stdin = self.stdin.as_mut().ok_or("the session is shut down")?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<Vec<Value>, Box<dyn Error>> {
        let response = self.request("tools/list", &json!({}))?;
        Ok(response
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .ok_or_else(|| format!("no tools in {response}"))?
            .clone())
    }

    /// Call a tool and return the text of its first content block, plus
    /// whether the result was flagged `isError`.
    fn call(&mut self, name: &str, arguments: &Value) -> Result<(String, bool), Box<dyn Error>> {
        let response = self.request(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        )?;
        let result = response
            .get("result")
            .ok_or_else(|| format!("no result in {response}"))?;
        let failed = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no text content in {result}"))?
            .to_owned();
        Ok((text, failed))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing stdin gives the server EOF, which is how a stdio
        // transport is told the session is over. It exits on its own, and
        // only then does it write out what it executed.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Every field `tools/list` puts on the wire for one tool, so a comparison
/// against the in-process surface does not silently ignore a schema
/// mismatch by comparing only names.
fn wire_shape(tool: &Value) -> Value {
    json!({
        "name": tool.get("name"),
        "description": tool.get("description"),
        "inputSchema": tool.get("inputSchema"),
    })
}

fn expected_wire_shape(spec: &silent_critic::tools::ToolSpec) -> Value {
    let schema = spec.schema.as_object().cloned().unwrap_or_default();
    json!({
        "name": spec.name,
        "description": spec.description,
        "inputSchema": schema,
    })
}

// -- acceptance check 1: listing matches the surface's specs exactly -------

#[test]
fn orchestrator_tools_list_matches_the_surfaces_specs_exactly() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let expected: Vec<Value> = orchestrator_tools(&fixture)
        .specs()
        .iter()
        .map(expected_wire_shape)
        .collect();

    let mut session = Session::start(&fixture, None)?;
    let actual: Vec<Value> = session.list_tools()?.iter().map(wire_shape).collect();

    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn worker_tools_list_matches_the_surfaces_specs_exactly() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (token, task) = mint_worker_token(&fixture, "T002")?;
    let registry_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?
        .parent()
        .ok_or("plan path has no parent")?
        .join("run")
        .join("tokens.toml");
    let registry = TokenRegistry::load(&registry_path)?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &Token::new(token.as_str().to_owned()),
    );
    let expected: Vec<Value> = worker.specs().iter().map(expected_wire_shape).collect();
    assert!(!expected.is_empty(), "the worker surface offered no tools");

    let mut session = Session::start(&fixture, Some(token.as_str()))?;
    let actual: Vec<Value> = session.list_tools()?.iter().map(wire_shape).collect();

    assert_eq!(actual, expected);
    Ok(())
}

// -- acceptance check 2: a call's wire response matches the surface's call -

#[test]
fn calling_plan_status_over_the_wire_matches_the_surfaces_own_call() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let expected = match orchestrator_tools(&fixture).call("plan_status", &json!({})) {
        ToolOutcome::Text(text) => text,
        ToolOutcome::Failed(reason) => return Err(reason.into()),
    };

    let mut session = Session::start(&fixture, None)?;
    let (actual, failed) = session.call("plan_status", &json!({}))?;

    assert!(!failed, "plan_status failed over the wire: {actual}");
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn calling_brief_over_the_wire_matches_the_surfaces_own_call() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (token, task) = mint_worker_token(&fixture, "T002")?;
    let registry_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?
        .parent()
        .ok_or("plan path has no parent")?
        .join("run")
        .join("tokens.toml");
    let registry = TokenRegistry::load(&registry_path)?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &Token::new(token.as_str().to_owned()),
    );
    let expected = match worker.call("brief", &json!({})) {
        ToolOutcome::Text(text) => text,
        ToolOutcome::Failed(reason) => return Err(reason.into()),
    };

    let mut session = Session::start(&fixture, Some(token.as_str()))?;
    let (actual, failed) = session.call("brief", &json!({}))?;

    assert!(!failed, "brief failed over the wire: {actual}");
    assert_eq!(actual, expected);
    Ok(())
}

// -- invariant: a Failed outcome is isError content, never a protocol error

#[test]
fn a_failed_outcome_arrives_as_error_content_not_a_protocol_error() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let mut session = Session::start(&fixture, None)?;

    let response = session.request(
        "tools/call",
        &json!({ "name": "dispatch", "arguments": { "task_id": "does-not-exist" } }),
    )?;

    assert!(
        response.get("error").is_none(),
        "a tool failure surfaced as a protocol error: {response}"
    );
    let (text, failed) = session.call("dispatch", &json!({ "task_id": "does-not-exist" }))?;
    assert!(failed, "an unready/unknown task dispatch succeeded: {text}");
    assert!(!text.is_empty(), "the failure carried no reason");
    Ok(())
}

#[test]
fn an_unauthenticated_worker_token_fails_readably_rather_than_refusing_to_start() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    // No registry has ever been saved for this plan, so any token is
    // unknown.
    let mut session = Session::start(&fixture, Some("silent_critic_worker_does-not-exist"))?;

    // The server started and answers `tools/list` (empty: no caller
    // authenticated) rather than refusing the connection.
    let tools = session.list_tools()?;
    assert!(
        tools.is_empty(),
        "an unauthenticated session saw tools: {tools:?}"
    );

    let (text, failed) = session.call("brief", &json!({}))?;
    assert!(failed, "an unauthenticated call to brief succeeded: {text}");
    assert!(!text.is_empty(), "the failure carried no reason");
    Ok(())
}

/// Fix round 2, finding #8: `dispatch` writes `SILENT_CRITIC_TASK_ID` into
/// `mcp.json`'s own env, and `silent-critic-mcp` must pass it straight through as
/// `Tools::worker`'s caller-declared `expected_task` -- a genuine second
/// factor a caller cannot satisfy merely by presenting a valid token minted
/// for some other task.
#[test]
fn a_valid_token_presented_with_a_mismatched_task_id_does_not_authenticate() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (token, _task) = mint_worker_token(&fixture, "T002")?;

    // The token genuinely resolves to T002; declaring a different expected
    // task must still fail to authenticate, since (before this fix) the
    // expected task was derived from the token itself and this mismatch
    // could never be detected.
    let mut session =
        Session::start_worker_with_task_id(&fixture, token.as_str(), "does-not-exist")?;

    let tools = session.list_tools()?;
    assert!(
        tools.is_empty(),
        "a task-id-mismatched worker session saw tools: {tools:?}"
    );
    let (text, failed) = session.call("brief", &json!({}))?;
    assert!(
        failed,
        "a call authenticated despite a mismatched SILENT_CRITIC_TASK_ID: {text}"
    );
    Ok(())
}

/// The matching-task-id counterpart of the mismatch test above: a worker
/// session whose `SILENT_CRITIC_TASK_ID` agrees with the token's own resolved
/// task still authenticates normally.
#[test]
fn a_valid_token_presented_with_a_matching_task_id_still_authenticates() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (token, _task) = mint_worker_token(&fixture, "T002")?;

    let mut session = Session::start_worker_with_task_id(&fixture, token.as_str(), "T002")?;

    let tools = session.list_tools()?;
    assert!(
        !tools.is_empty(),
        "a correctly task-id-scoped worker session saw no tools"
    );
    let (text, failed) = session.call("brief", &json!({}))?;
    assert!(!failed, "a correctly scoped worker call failed: {text}");
    Ok(())
}

/// Fix round 2, finding #2: before this fix, `silent-critic-mcp` always built its
/// `Tools` value with `harness: None` and never called `.with_judge`, so
/// `dispatch` and `judge` always answered "not configured" no matter what
/// an operator ran the server with. A `--config`/`SILENT_CRITIC_CONFIG` TOML file
/// supplying `[harness]` must make `dispatch` reach the configured harness
/// over the real wire, not merely in a `silent_critic::tools::Tools` value built
/// directly in-process.
#[test]
fn silent_critic_config_wires_a_harness_through_to_dispatch_over_the_wire() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;

    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("silent-critic-config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[harness]
program = "/bin/sh"
args = ["-c", "true"]
prompt_via = "brief_path"
shell = "/bin/sh"
timeout_secs = 30
silent_critic_mcp_path = "{silent_critic_mcp}"
"#,
            silent_critic_mcp = env!("CARGO_BIN_EXE_silent-critic-mcp"),
        ),
    )?;

    let mut session = Session::start_orchestrator_with_config(&fixture, &config_path)?;
    let (text, failed) = session.call("dispatch", &json!({ "task_id": "T001" }))?;

    assert!(
        !failed,
        "dispatch failed despite a configured harness: {text}"
    );
    assert!(
        !text.contains("no harness is configured"),
        "dispatch still reported no harness configured: {text}"
    );
    Ok(())
}

#[test]
fn an_unknown_tool_is_refused_by_name() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let mut session = Session::start(&fixture, None)?;
    let (text, failed) = session.call("delete_everything", &json!({}))?;
    assert!(failed, "an unknown tool succeeded: {text}");
    assert!(text.contains("delete_everything"), "{text}");
    Ok(())
}

/// Without `SILENT_CRITIC_REPO`, the process edge falls back to its own current
/// directory, mirroring `silent-critic`'s own CLI.
#[test]
fn without_silent_critic_repo_the_session_supervises_its_own_current_directory() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let expected = match orchestrator_tools(&fixture).call("plan_status", &json!({})) {
        ToolOutcome::Text(text) => text,
        ToolOutcome::Failed(reason) => return Err(reason.into()),
    };

    let mut session = Session::start_with_repo_defaulted_to_cwd(&fixture)?;
    let (actual, failed) = session.call("plan_status", &json!({}))?;

    assert!(!failed, "plan_status failed over the wire: {actual}");
    assert_eq!(actual, expected);
    Ok(())
}

/// Without `SILENT_CRITIC_STORE_ROOT`, the process edge falls back to
/// `default_store_root`, exactly like `silent-critic`'s own CLI. This puts the
/// fixture's store where that default resolves to
/// (`<HOME>/.local/share/silent-critic`)
/// rather than in an arbitrary temp directory, so the fallback computation
/// itself is exercised rather than bypassed.
#[test]
fn without_silent_critic_store_root_the_session_falls_back_to_the_xdg_default() -> TestResult {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-fixture-plan.md");
    std::fs::write(&plan_path, STORE_PLAN)?;

    let home_dir = tempfile::tempdir()?;
    let store_root =
        default_store_root(None, Some(home_dir.path())).ok_or("no default store root")?;
    let store = Store::new(StoreRoot::new(store_root));
    let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

    let expected = match Tools::orchestrator(
        &store,
        repo_dir.path().to_path_buf(),
        plan_id.as_str().to_owned(),
        None,
    )
    .call("plan_status", &json!({}))
    {
        ToolOutcome::Text(text) => text,
        ToolOutcome::Failed(reason) => return Err(reason.into()),
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_silent-critic-mcp"))
        .env("HOME", home_dir.path())
        .env("SILENT_CRITIC_REPO", repo_dir.path())
        .env("SILENT_CRITIC_PLAN_ID", plan_id.as_str())
        .env_remove("SILENT_CRITIC_STORE_ROOT")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().ok_or("no stdin on the server")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout on the server")?);
    let mut session = Session {
        child,
        stdin: Some(stdin),
        stdout,
        next_id: 1,
    };
    session.request(
        "initialize",
        &json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "silent-critic-tests", "version": "0" },
        }),
    )?;
    session.notify("notifications/initialized", &json!({}))?;

    let (actual, failed) = session.call("plan_status", &json!({}))?;

    assert!(!failed, "plan_status failed over the wire: {actual}");
    assert_eq!(actual, expected);
    Ok(())
}

/// A client that never completes the `initialize` handshake gives
/// `silent_critic::mcp_stdio::serve` an error, which `main` must report readably
/// (a nonzero exit and a message on stderr) rather than panicking.
/// Exercised for both scopes: the orchestrator and worker branches of
/// `main`'s `match cli.token` translate `serve`'s error identically, but as
/// separate source expressions, and both must run to be provably correct
/// (`REPO_INVARIANTS.md` RS-007).
fn assert_reports_a_broken_handshake_readably(
    fixture: &Fixture,
    token: Option<&str>,
) -> TestResult {
    let mut command = Command::new(env!("CARGO_BIN_EXE_silent-critic-mcp"));
    command
        .env("SILENT_CRITIC_STORE_ROOT", fixture.store_dir.path())
        .env("SILENT_CRITIC_REPO", &fixture.repo_start)
        .env("SILENT_CRITIC_PLAN_ID", &fixture.plan_id)
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME");
    match token {
        Some(token) => {
            command.env("SILENT_CRITIC_TOKEN", token);
        }
        None => {
            command.env_remove("SILENT_CRITIC_TOKEN");
        }
    }
    let output = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            // Not valid JSON-RPC, and stdin closes right after: the
            // transport never sees a completed `initialize` request.
            child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("no stdin on the server"))?
                .write_all(b"not json at all\n")?;
            child.wait_with_output()
        })?;

    assert!(
        !output.status.success(),
        "a broken handshake was reported as success"
    );
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        !stderr.is_empty(),
        "a broken handshake was reported with no diagnostic on stderr"
    );
    Ok(())
}

#[test]
fn an_orchestrator_session_reports_a_broken_handshake_readably() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    assert_reports_a_broken_handshake_readably(&fixture, None)
}

#[test]
fn a_worker_session_reports_a_broken_handshake_readably() -> TestResult {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (token, _task) = mint_worker_token(&fixture, "T002")?;
    assert_reports_a_broken_handshake_readably(&fixture, Some(token.as_str()))
}

/// A `tools/call` request may omit `arguments` entirely (it is optional on
/// the wire); the adapter must default it to `{}` rather than requiring
/// every client to send an empty object explicitly.
#[test]
fn a_tool_call_with_no_arguments_field_defaults_to_an_empty_object() -> TestResult {
    let fixture = set_up(STORE_PLAN)?;
    let mut session = Session::start(&fixture, None)?;

    let response = session.request("tools/call", &json!({ "name": "plan_status" }))?;

    let result = response
        .get("result")
        .ok_or_else(|| format!("no result in {response}"))?;
    assert_ne!(
        result.get("isError").and_then(Value::as_bool),
        Some(true),
        "plan_status failed with no arguments field: {result}"
    );
    Ok(())
}
