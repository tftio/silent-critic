//! CLI-surface tests for the `silent-critic-mcp` binary that don't require a full
//! protocol session (see `tests/mcp_stdio.rs` for those).

use assert_cmd::Command;

#[test]
fn mcp_cli_reports_its_version() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("silent-critic-mcp")?;

    let output = command.arg("--version").output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.starts_with("silent-critic-mcp "),
        "unexpected version output: {stdout}"
    );

    Ok(())
}

/// `--plan-id` (`SILENT_CRITIC_PLAN_ID`) is the one required argument the process
/// edge cannot default: clap refuses to start rather than serving a session
/// for no plan.
#[test]
fn mcp_cli_requires_a_plan_id() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("silent-critic-mcp")?;
    command.env_remove("SILENT_CRITIC_PLAN_ID");

    let output = command.output()?;

    assert!(!output.status.success(), "missing --plan-id was accepted");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("--plan-id"),
        "the missing argument was not named: {stderr}"
    );

    Ok(())
}

/// With `--plan-id` supplied but none of `SILENT_CRITIC_STORE_ROOT`,
/// `XDG_DATA_HOME`, or `HOME` available, the process edge cannot compute a
/// default store root and must report that plainly rather than panicking or
/// picking an arbitrary directory.
#[test]
fn mcp_cli_reports_store_root_error_when_no_source_is_available()
-> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("silent-critic-mcp")?;
    command
        .env_remove("SILENT_CRITIC_STORE_ROOT")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .arg("--plan-id")
        .arg("does-not-matter");

    let output = command.output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("cannot determine a store root"), "{stderr}");

    Ok(())
}
