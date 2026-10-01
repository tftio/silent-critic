//! Integration tests for `silent_critic::config`, driven entirely through the
//! crate's public API.
//!
//! Lives under `tests/` (not only inline in `src/config.rs`) so this
//! module's containment is exercised through the same compiled artifact a
//! consuming binary (`silent-critic-mcp`) would use, not only the crate's own
//! `--cfg test` build -- mirroring `tests/tools.rs`'s own module docs on
//! why a sentinel/containment test lives here rather than only inline.

use std::error::Error;

use silent_critic::config::{ConfigError, SilentCriticConfig};
use silent_critic::provider::PromptVia as ProviderPromptVia;
use silent_critic::tools::JudgeProviders;
use silent_critic::worktree::GitEnv;

fn test_git_env() -> GitEnv {
    GitEnv::new("git".to_owned(), String::new(), String::new())
}

#[test]
fn a_full_config_file_builds_a_harness_and_a_dual_judge_panel() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(
        tmp.path(),
        r#"
[harness]
program = "claude"
args = ["-p"]
model = "claude-sonnet-4-5"
model_flag = "--model"
prompt_via = "brief_path"
shell = "/bin/sh"
timeout_secs = 1800
silent_critic_mcp_path = "/usr/local/bin/silent-critic-mcp"

[judge]
retry_budget = 2
git_binary = "git"
path = "/usr/bin:/bin"
home = "/home/operator"
check_shell = "/bin/sh"
check_timeout_secs = 60

[[judge.providers]]
id = "judge-a"
program = "claude"
args = ["-p"]
prompt_via = "argument"
timeout_secs = 120

[[judge.providers]]
id = "judge-b"
program = "opencode"
args = ["run"]
prompt_via = "stdin"
timeout_secs = 120
"#,
    )?;

    let config = SilentCriticConfig::load(tmp.path())?;
    let harness = config
        .harness_spec(test_git_env())
        .ok_or("expected a harness section")?;
    assert_eq!(std::path::Path::new("claude"), harness.program);

    let judge = config.judge_config()?.ok_or("expected a judge section")?;
    match judge.providers {
        JudgeProviders::Dual(a, b) => {
            assert_eq!(ProviderPromptVia::Argument, a.prompt_via);
            assert_eq!(ProviderPromptVia::Stdin, b.prompt_via);
        }
        JudgeProviders::Single(_) => return Err("expected a dual judge panel".into()),
    }

    Ok(())
}

#[test]
fn a_config_file_with_neither_section_configures_nothing() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), "")?;

    let config = SilentCriticConfig::load(tmp.path())?;
    assert!(config.harness_spec(test_git_env()).is_none());
    assert!(config.judge_config()?.is_none());

    Ok(())
}

#[test]
fn a_single_judge_provider_builds_a_single_panel() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(
        tmp.path(),
        r#"
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
"#,
    )?;

    let config = SilentCriticConfig::load(tmp.path())?;
    let judge = config.judge_config()?.ok_or("expected a judge section")?;
    assert!(matches!(judge.providers, JudgeProviders::Single(_)));

    Ok(())
}

#[test]
fn a_bad_provider_count_is_a_typed_error() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(
        tmp.path(),
        r#"
[judge]
git_binary = "git"
path = ""
home = ""
check_shell = "/bin/sh"
check_timeout_secs = 30
providers = []
"#,
    )?;

    let config = SilentCriticConfig::load(tmp.path())?;
    let actual = config.judge_config();
    assert!(matches!(actual, Err(ConfigError::ProviderCount(0))));

    Ok(())
}

#[test]
fn a_missing_file_is_a_typed_io_error() {
    let actual =
        SilentCriticConfig::load(std::path::Path::new("/no/such/silent-critic-config.toml"));
    assert!(matches!(actual, Err(ConfigError::Io { .. })));
}

#[test]
fn malformed_toml_is_a_typed_parse_error() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), "not = [valid toml")?;

    let actual = SilentCriticConfig::load(tmp.path());
    assert!(matches!(actual, Err(ConfigError::Parse { .. })));

    Ok(())
}
