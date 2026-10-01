//! Process-edge TOML configuration for the harness and judge (fix round 2,
//! finding #2).
//!
//! Before this module existed, `src/bin/silent-critic-mcp.rs` always constructed
//! its `Tools` value with `harness: None` and never called `.with_judge`,
//! so the `dispatch` and `judge` MCP tools always answered "not configured"
//! and the supervision loop this whole crate exists to run was unreachable
//! over MCP. This module is the typed parse target for an optional TOML
//! file (`--config`/`SILENT_CRITIC_CONFIG`, read once at the process edge per
//! RS-008) that supplies both: `[harness]` becomes a
//! [`crate::dispatch::HarnessSpec`], `[judge]` becomes a
//! [`crate::tools::JudgeConfig`]. Absence of the file (or of either table
//! inside it) keeps today's "not configured" behavior -- this module never
//! invents a harness or judge configuration on its own.
//!
//! The harness's worktree-creation git environment (`GitEnv`: the git
//! binary, `PATH`, `HOME`) is deliberately *not* part of `[harness]`: it is
//! supplied by the caller, exactly as `src/bin/silent-critic.rs`'s manual dispatch
//! already reads it from CLI flags/`clap`'s `env` attribute, so this module
//! never reads `std::env` itself and never duplicates that CLI surface.
//! `[judge]`'s `git_binary`/`path`/`home` are different: they configure a
//! separate `GitInvocation` (fact capture from the dispatched worktree, not
//! worktree creation) that is configured per judge rather than per
//! invocation, so they belong in this file and are parsed here.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::dispatch::{DispatchConfig, HarnessSpec, PromptVia as HarnessPromptVia};
use crate::evaluate::automated::{CheckEnvironment, CheckTimeout};
use crate::evaluate::judge::RetryBudget;
use crate::git::GitInvocation;
use crate::provider::{CommandEnvironment, CommandProviderSpec, PromptVia as ProviderPromptVia};
use crate::tools::{JudgeConfig, JudgeProviders};
use crate::worktree::GitEnv;

/// A loaded, still-typed configuration file: `[harness]` and/or `[judge]`,
/// either or both absent.
///
/// Deliberately not consumed by [`SilentCriticConfig::harness_spec`] or
/// [`SilentCriticConfig::judge_config`]: a caller may want both from the same
/// loaded file, so both take `&self` and clone the small amount of raw data
/// each needs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SilentCriticConfig {
    /// The harness to launch for a dispatched worker, if configured.
    harness: Option<RawHarness>,
    /// The judge provider(s) to consult, if configured.
    judge: Option<RawJudge>,
}

impl SilentCriticConfig {
    /// Read and parse the TOML configuration file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when `path` cannot be read, or
    /// [`ConfigError::Parse`] when its contents are not a valid
    /// `SilentCriticConfig` document.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let body = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&body).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Build the configured [`HarnessSpec`], if `[harness]` was present.
    ///
    /// `git` is the worktree-creation environment the caller already reads
    /// from its own CLI flags/environment (see the module docs for why
    /// that is not part of `[harness]`); `silent_critic_mcp_path` inside
    /// `[harness]` still becomes `DispatchConfig::silent_critic_mcp_path`.
    #[must_use]
    pub fn harness_spec(&self, git: GitEnv) -> Option<HarnessSpec> {
        self.harness.clone().map(|raw| raw.into_harness_spec(git))
    }

    /// Build the configured [`JudgeConfig`], if `[judge]` was present.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ProviderCount`] when `[judge]`'s `providers`
    /// array does not have exactly one or two entries.
    pub fn judge_config(&self) -> Result<Option<JudgeConfig>, ConfigError> {
        self.judge
            .clone()
            .map(RawJudge::into_judge_config)
            .transpose()
    }
}

/// Failures loading or interpreting a [`SilentCriticConfig`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("reading config {path}: {source}")]
    Io {
        /// The configuration file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The configuration file could not be parsed as TOML matching this
    /// crate's schema.
    #[error("parsing config {path}: {source}")]
    Parse {
        /// The configuration file that could not be parsed.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// `[judge]`'s `providers` array had a length other than one or two.
    #[error("[judge].providers must have exactly one or two entries, found {0}")]
    ProviderCount(usize),
}

/// How a harness expects the worker's prompt delivered, as written in
/// `[harness].prompt_via`. A distinct type from [`RawProviderPromptVia`]
/// (`REPO_INVARIANTS.md` ENG-009): a harness can never be configured with
/// `stdin`, which [`crate::dispatch::PromptVia`] does not represent either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawHarnessPromptVia {
    Argument,
    BriefPath,
}

impl From<RawHarnessPromptVia> for HarnessPromptVia {
    fn from(value: RawHarnessPromptVia) -> Self {
        match value {
            RawHarnessPromptVia::Argument => Self::Argument,
            RawHarnessPromptVia::BriefPath => Self::BriefPath,
        }
    }
}

/// How a judge provider expects the prompt delivered, as written in
/// `[[judge.providers]].prompt_via`. See [`RawHarnessPromptVia`] for why
/// this is a separate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawProviderPromptVia {
    Stdin,
    Argument,
}

impl From<RawProviderPromptVia> for ProviderPromptVia {
    fn from(value: RawProviderPromptVia) -> Self {
        match value {
            RawProviderPromptVia::Stdin => Self::Stdin,
            RawProviderPromptVia::Argument => Self::Argument,
        }
    }
}

/// `[harness]`: the TOML shape of a [`HarnessSpec`], minus the
/// worktree-creation git environment (see the module docs).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHarness {
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_flag: Option<String>,
    prompt_via: RawHarnessPromptVia,
    shell: PathBuf,
    timeout_secs: u64,
    silent_critic_mcp_path: PathBuf,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

impl RawHarness {
    fn into_harness_spec(self, git: GitEnv) -> HarnessSpec {
        HarnessSpec {
            config: DispatchConfig {
                git,
                silent_critic_mcp_path: self.silent_critic_mcp_path,
            },
            program: self.program,
            args: self.args,
            model: self.model,
            model_flag: self.model_flag,
            prompt_via: self.prompt_via.into(),
            environment: self.environment.into_iter().collect(),
            shell: self.shell,
            timeout: CheckTimeout::new(Duration::from_secs(self.timeout_secs)),
        }
    }
}

/// `[[judge.providers]]`: the TOML shape of one [`CommandProviderSpec`].
///
/// `id` is a human-readable label only (e.g. distinguishing `claude -p`
/// from `opencode run` in the file itself); the tool surface mints its own
/// internal `judge-a`/`judge-b` provider identifiers regardless of what a
/// caller writes here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    // A human-readable label only, required so each provider entry in the
    // file is identifiable, in the documented shape (`{ id, program, args, model, model_flag,
    // prompt_via, timeout_secs, environment }`); never consulted, since
    // `Tools::orchestrator`'s judge panel mints its own `judge-a`/`judge-b`
    // provider identifiers regardless of what a caller writes here.
    #[allow(dead_code)]
    id: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_flag: Option<String>,
    prompt_via: RawProviderPromptVia,
    timeout_secs: u64,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

impl RawProvider {
    fn into_spec(self) -> CommandProviderSpec {
        let environment = self
            .environment
            .into_iter()
            .fold(CommandEnvironment::default(), |env, (name, value)| {
                env.with(name, value)
            });
        CommandProviderSpec {
            program: self.program,
            args: self.args,
            model_flag: self.model_flag,
            model: self.model,
            prompt_via: self.prompt_via.into(),
            environment,
            timeout: Duration::from_secs(self.timeout_secs),
        }
    }
}

/// `[judge]`: the TOML shape of a [`JudgeConfig`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJudge {
    providers: Vec<RawProvider>,
    #[serde(default)]
    retry_budget: Option<u32>,
    git_binary: String,
    path: String,
    home: String,
    check_shell: PathBuf,
    check_timeout_secs: u64,
    #[serde(default)]
    check_environment: BTreeMap<String, String>,
}

impl RawJudge {
    fn into_judge_config(self) -> Result<JudgeConfig, ConfigError> {
        let provider_count = self.providers.len();
        let mut providers = self.providers.into_iter();
        let providers = match (providers.next(), providers.next(), providers.next()) {
            (Some(first), None, None) => JudgeProviders::Single(first.into_spec()),
            (Some(first), Some(second), None) => {
                JudgeProviders::Dual(first.into_spec(), second.into_spec())
            }
            _ => return Err(ConfigError::ProviderCount(provider_count)),
        };
        let check_environment = self
            .check_environment
            .into_iter()
            .fold(CheckEnvironment::new(), |env, (name, value)| {
                env.with(name, value)
            });
        Ok(JudgeConfig {
            providers,
            retry_budget: self
                .retry_budget
                .map_or_else(RetryBudget::default, RetryBudget::new),
            git: GitInvocation::new(self.git_binary, self.path, self.home),
            check_shell: self.check_shell,
            check_environment,
            check_timeout: CheckTimeout::new(Duration::from_secs(self.check_timeout_secs)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, SilentCriticConfig};
    use crate::provider::PromptVia as ProviderPromptVia;
    use crate::tools::JudgeProviders;
    use crate::worktree::GitEnv;

    const FULL_EXAMPLE: &str = r#"
[harness]
program = "claude"
args = ["-p"]
model = "claude-sonnet-4-5"
model_flag = "--model"
prompt_via = "argument"
shell = "/bin/sh"
timeout_secs = 1800
silent_critic_mcp_path = "/usr/local/bin/silent-critic-mcp"

[harness.environment]
HARNESS_VAR = "harness-value"

[judge]
retry_budget = 3
git_binary = "git"
path = "/usr/bin:/bin"
home = "/home/operator"
check_shell = "/bin/sh"
check_timeout_secs = 60

[judge.check_environment]
JUDGE_VAR = "judge-value"

[[judge.providers]]
id = "judge-a"
program = "claude"
args = ["-p"]
model = "claude-opus-4"
model_flag = "--model"
prompt_via = "argument"
timeout_secs = 120

[judge.providers.environment]
PROVIDER_VAR = "provider-value"

[[judge.providers]]
id = "judge-b"
program = "opencode"
args = ["run"]
prompt_via = "stdin"
timeout_secs = 120
"#;

    fn test_git_env() -> GitEnv {
        GitEnv::new("git".to_owned(), String::new(), String::new())
    }

    #[test]
    fn full_example_parses_a_harness_and_a_dual_judge() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), FULL_EXAMPLE)?;

        let config = SilentCriticConfig::load(tmp.path())?;

        let harness = config
            .harness_spec(test_git_env())
            .ok_or("expected a harness section")?;
        assert_eq!(std::path::Path::new("claude"), harness.program);
        assert_eq!(
            Some("/usr/local/bin/silent-critic-mcp"),
            harness.config.silent_critic_mcp_path.to_str()
        );
        assert_eq!(
            Some(&("HARNESS_VAR".to_owned(), "harness-value".to_owned())),
            harness.environment.first()
        );

        let judge = config.judge_config()?.ok_or("expected a judge section")?;
        assert_eq!(3, judge.retry_budget.retries());
        // A single `assert!(matches!(...))` rather than a `match`/`let-else`
        // with a "not Dual" arm: any such arm is dead code on every
        // passing run of this test (the fixture always builds a `Dual`
        // panel), and this crate's coverage gate tracks lines inside
        // `#[cfg(test)]` modules too -- `matches!`'s generated match arms
        // are not attributed to a line of this module's own source.
        assert!(matches!(
            &judge.providers,
            JudgeProviders::Dual(a, b)
                if a.program == "claude"
                    && a.prompt_via == ProviderPromptVia::Argument
                    && a.environment.pairs().first()
                        == Some(&("PROVIDER_VAR".to_owned(), "provider-value".to_owned()))
                    && b.program == "opencode"
                    && b.prompt_via == ProviderPromptVia::Stdin
        ));

        Ok(())
    }

    #[test]
    fn absent_sections_yield_no_harness_and_no_judge() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), "")?;

        let config = SilentCriticConfig::load(tmp.path())?;

        assert!(config.harness_spec(test_git_env()).is_none());
        assert!(config.judge_config()?.is_none());

        Ok(())
    }

    #[test]
    fn single_provider_yields_a_single_judge_panel() -> Result<(), Box<dyn std::error::Error>> {
        let body = r#"
[judge]
git_binary = "git"
path = ""
home = ""
check_shell = "/bin/sh"
check_timeout_secs = 30

[[judge.providers]]
id = "solo"
program = "claude"
prompt_via = "argument"
timeout_secs = 30
"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), body)?;

        let config = SilentCriticConfig::load(tmp.path())?;
        let judge = config.judge_config()?.ok_or("expected a judge section")?;
        assert!(matches!(judge.providers, JudgeProviders::Single(_)));
        // Default retry budget when `retry_budget` is omitted.
        assert_eq!(2, judge.retry_budget.retries());

        Ok(())
    }

    #[test]
    fn zero_providers_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let body = r#"
[judge]
git_binary = "git"
path = ""
home = ""
check_shell = "/bin/sh"
check_timeout_secs = 30
providers = []
"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), body)?;

        let config = SilentCriticConfig::load(tmp.path())?;
        let actual = config.judge_config();

        assert!(matches!(actual, Err(ConfigError::ProviderCount(0))));

        Ok(())
    }

    #[test]
    fn three_providers_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let body = r#"
[judge]
git_binary = "git"
path = ""
home = ""
check_shell = "/bin/sh"
check_timeout_secs = 30

[[judge.providers]]
id = "a"
program = "claude"
prompt_via = "argument"
timeout_secs = 30

[[judge.providers]]
id = "b"
program = "claude"
prompt_via = "argument"
timeout_secs = 30

[[judge.providers]]
id = "c"
program = "claude"
prompt_via = "argument"
timeout_secs = 30
"#;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), body)?;

        let config = SilentCriticConfig::load(tmp.path())?;
        let actual = config.judge_config();

        assert!(matches!(actual, Err(ConfigError::ProviderCount(3))));

        Ok(())
    }

    #[test]
    fn missing_file_reports_an_io_error() {
        let actual = SilentCriticConfig::load(Path::new("/no/such/silent-critic-config.toml"));
        assert!(matches!(actual, Err(ConfigError::Io { .. })));
    }

    #[test]
    fn malformed_toml_reports_a_parse_error() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), "not = [valid toml")?;

        let actual = SilentCriticConfig::load(tmp.path());

        assert!(matches!(actual, Err(ConfigError::Parse { .. })));

        Ok(())
    }

    #[test]
    fn unknown_top_level_key_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), "unknown_section = true\n")?;

        let actual = SilentCriticConfig::load(tmp.path());

        assert!(matches!(actual, Err(ConfigError::Parse { .. })));

        Ok(())
    }

    use std::path::Path;
}
