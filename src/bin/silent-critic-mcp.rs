#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)
)]
//! Stdio MCP server entry point for `silent-critic`.
//!
//! This binary is the process edge (`REPO_INVARIANTS.md` CLI-001, RS-008):
//! it reads `SILENT_CRITIC_STORE_ROOT` (falling back to `XDG_DATA_HOME`/`HOME`,
//! computed the way `src/bin/silent-critic.rs` does), `SILENT_CRITIC_REPO`,
//! `SILENT_CRITIC_PLAN_ID`, and `SILENT_CRITIC_TOKEN` once via clap's `env` attribute,
//! builds a [`silent_critic::tools::Tools`] value from them, and hands it to
//! [`silent_critic::mcp_stdio::serve`]. Everything an agent can ask for is decided
//! in `silent_critic::tools`, and how it is carried in `silent_critic::mcp_stdio`; this
//! binary decides neither.
//!
//! **stdout is the protocol.** A stray `println!` here would be framed as a
//! JSON-RPC message and break the session, which is why diagnostics go to
//! stderr, mirroring `silent_critic::store`'s own binary.
//!
//! **The token names its own scope.** `SILENT_CRITIC_TOKEN` absent means this
//! session is the orchestrator's own (no token needed — the MCP session
//! itself is the operator's, per T005's brief); present, it is validated
//! against the plan's disposable token sidecar
//! (`<plan_dir>/run/tokens.toml`, `silent_critic::token::TokenRegistry`) and the
//! session is scoped to whichever task the token itself resolves to. A
//! token that does not validate still produces a running server — every
//! call then fails with a readable reason (see `silent_critic::tools` module
//! docs), never a refusal to start, since a worker session with a bad token
//! is a fact the agent calling it needs to read, not a crash the operator
//! has to diagnose from a process exit code.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use silent_critic::config::SilentCriticConfig;
use silent_critic::model::{PlanId, TaskId};
use silent_critic::store::{Store, StoreRoot, default_store_root};
use silent_critic::token::{Role, Token, TokenRegistry};
use silent_critic::tools::Tools;
use silent_critic::worktree::GitEnv;

/// Command-line arguments for the `silent-critic-mcp` stdio server.
#[derive(Debug, Parser)]
#[command(
    name = "silent-critic-mcp",
    author,
    version,
    about = "Stdio MCP server for the silent-critic supervision loop"
)]
struct Cli {
    /// The plan store's root directory. Defaults to `$XDG_DATA_HOME/silent-critic`
    /// or `~/.local/share/silent-critic`, computed from `XDG_DATA_HOME`/`HOME` the
    /// way `silent-critic`'s own CLI does.
    #[arg(long, env = "SILENT_CRITIC_STORE_ROOT")]
    store_root: Option<PathBuf>,

    /// The XDG data directory; read only to compute the default store root
    /// when `SILENT_CRITIC_STORE_ROOT` is unset.
    #[arg(long, env = "XDG_DATA_HOME", hide_env_values = true)]
    xdg_data_home: Option<PathBuf>,

    /// The user's home directory; read only to compute the default store
    /// root when `SILENT_CRITIC_STORE_ROOT` is unset and `XDG_DATA_HOME` is also
    /// unset.
    #[arg(long, env = "HOME", hide_env_values = true)]
    home: Option<PathBuf>,

    /// The repository (or worktree) this session supervises. Defaults to
    /// the current directory.
    #[arg(long, env = "SILENT_CRITIC_REPO")]
    repo: Option<PathBuf>,

    /// The plan this session serves.
    #[arg(long, env = "SILENT_CRITIC_PLAN_ID")]
    plan_id: String,

    /// A worker bearer token. Present means this session is scoped to that
    /// token's task; absent means the orchestrator's own scope.
    #[arg(long, env = "SILENT_CRITIC_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// The task this worker session is for, as `dispatch` (`src/dispatch.rs`)
    /// writes it into `mcp.json`'s own `env` block (fix round 2, finding
    /// #8). Preferred over deriving the expected task from the presented
    /// token itself, which made `Tools::worker`'s caller-declared/
    /// registry-resolved check compare a value against itself. Absent
    /// (legacy `mcp.json` written before this fix, or a hand-built
    /// worker session) falls back to the token-derived task.
    #[arg(long, env = "SILENT_CRITIC_TASK_ID")]
    task_id: Option<String>,

    /// A TOML file supplying `[harness]` and/or `[judge]` configuration
    /// (see `README.md`). Read once here at the process edge, never inside
    /// `silent_critic::tools`; absent means the pre-existing "not configured"
    /// behavior for both `dispatch` and `judge` (fix round 2, finding #2).
    #[arg(long, env = "SILENT_CRITIC_CONFIG")]
    config: Option<PathBuf>,

    /// The git binary a configured harness's worktree creation invokes.
    /// Only consulted when `[harness]` is present in `--config`.
    #[arg(long, default_value = "git")]
    git_binary: String,

    /// `PATH` for the harness's worktree-creation git subprocess; read
    /// once here per RS-008 and never inside the library.
    #[arg(long, env = "PATH", hide_env_values = true, default_value = "")]
    git_path: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let store_root = cli
        .store_root
        .or_else(|| default_store_root(cli.xdg_data_home.as_deref(), cli.home.as_deref()))
        .context(
            "cannot determine a store root: set SILENT_CRITIC_STORE_ROOT, XDG_DATA_HOME, or HOME",
        )?;
    // Leaked deliberately: `Tools<'store>` borrows the store for its whole
    // lifetime, and this process serves exactly one MCP session for its
    // whole lifetime, so a `'static` reference costs nothing this process
    // was not already going to hold until exit.
    let store: &'static Store = Box::leak(Box::new(Store::new(StoreRoot::new(store_root))));

    let repo = match cli.repo {
        Some(repo) => repo,
        None => std::env::current_dir().context("determine current directory")?,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    // Read once here, at the process edge (RS-008): absence keeps today's
    // "not configured" behavior for both `dispatch` and `judge` (fix round
    // 2, finding #2).
    let (harness, judge) = match &cli.config {
        None => (None, None),
        Some(config_path) => {
            let config = SilentCriticConfig::load(config_path)
                .with_context(|| format!("loading config {}", config_path.display()))?;
            let git = GitEnv::new(
                cli.git_binary.clone(),
                cli.git_path.clone(),
                cli.home
                    .clone()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
            let harness = config.harness_spec(git);
            let judge = config
                .judge_config()
                .with_context(|| format!("interpreting [judge] in {}", config_path.display()))?;
            (harness, judge)
        }
    };

    match cli.token {
        None => {
            let mut tools = Tools::orchestrator(store, repo, cli.plan_id, harness);
            if let Some(judge) = judge {
                tools = tools.with_judge(judge);
            }
            runtime
                .block_on(silent_critic::mcp_stdio::serve(tools))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Some(raw_token) => {
            let (registry, task, token) =
                resolve_worker_session(store, &repo, &cli.plan_id, raw_token, cli.task_id);
            let tools = Tools::worker(store, repo, cli.plan_id, &registry, &task, &token);
            runtime
                .block_on(silent_critic::mcp_stdio::serve(tools))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    Ok(())
}

/// Resolve a presented `SILENT_CRITIC_TOKEN` into the registry, task, and
/// [`Token`] value [`Tools::worker`] needs.
///
/// `expected_task_id` is `dispatch`'s own `SILENT_CRITIC_TASK_ID`
/// (`src/dispatch.rs`'s `mcp_env`), present on every `mcp.json` this crate
/// writes going forward: it is used directly as `Tools::worker`'s
/// caller-declared `expected_task`, a genuine second factor the presented
/// token's own resolution must still agree with (fix round 2, finding #8).
/// `None` (a legacy `mcp.json` written before this fix, or a hand-built
/// worker session) falls back to deriving the expected task from the token
/// itself, exactly as before -- documented as legacy, since it makes that
/// check compare a value against its own resolution. A token that does not
/// resolve (no sidecar, unreadable sidecar, or unknown token) still returns
/// a value: `Tools::worker` then produces an unauthenticated surface whose
/// every call fails with a readable reason, rather than a binary that
/// refuses to start.
fn resolve_worker_session(
    store: &Store,
    repo: &std::path::Path,
    plan_id: &str,
    raw_token: String,
    expected_task_id: Option<String>,
) -> (TokenRegistry, TaskId, Token) {
    let token = Token::new(raw_token);
    let registry = tokens_registry_path(store, repo, plan_id)
        .and_then(|path| TokenRegistry::load(&path).ok())
        .unwrap_or_else(|| TokenRegistry::new(PlanId::new(plan_id.to_owned())));
    let task = expected_task_id.map_or_else(
        || match registry.validate(&token) {
            Some(Role::Worker { task }) => task.clone(),
            _ => TaskId::new(String::new()),
        },
        TaskId::new,
    );
    (registry, task, token)
}

/// The path to a plan's disposable token sidecar
/// (`<plan_dir>/run/tokens.toml`), or `None` when the plan itself cannot be
/// located.
fn tokens_registry_path(store: &Store, repo: &std::path::Path, plan_id: &str) -> Option<PathBuf> {
    let plan_path = store.plan_path(repo, plan_id).ok()?;
    let plan_dir = plan_path.parent()?;
    Some(plan_dir.join("run").join("tokens.toml"))
}
