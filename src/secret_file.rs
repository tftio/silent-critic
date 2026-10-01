//! A single, shared helper for writing a file that carries a bearer token
//! (or any other local secret) in the clear (fix round 2, finding #7).
//!
//! Before this module existed, `src/dispatch.rs` restricted `mcp.json` to
//! owner-only permissions (`0o600` on Unix) with its own private
//! `write_secret_file`, while `src/token.rs`'s `TokenRegistry::save` wrote
//! `tokens.toml` -- which holds every worker token in cleartext -- through
//! a plain `std::fs::write` at whatever permissions the process umask
//! leaves it with. Both files carry the same kind of secret and belong to
//! the same trust boundary (the plan's own `run/` sidecar directory, owned
//! by the operator); this module is their one shared implementation, so a
//! future secret-bearing sidecar reaches for this rather than reintroducing
//! a third, possibly-forgotten `0o600` write.

use std::fs;
use std::path::Path;

/// Write `body` to `path`, restricted to owner read/write (`0o600`) on
/// Unix.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] when `path` cannot be opened
/// (or created) or `body` cannot be written.
#[cfg(unix)]
pub fn write_secret_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)
}

/// Non-Unix fallback: plain-permission write. No target in this
/// repository's CI matrix is non-Unix (`REPO_INVARIANTS.md` lists only
/// `linux-amd64`/`linux-arm64`/`macos-arm64`), so this is never compiled
/// there and carries no coverage obligation on this platform.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] when `body` cannot be
/// written to `path`.
#[cfg(not(unix))]
pub fn write_secret_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    fs::write(path, body)
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_secret_file;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn writes_owner_only_permissions() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("secret.toml");

        write_secret_file(&path, b"silent_critic_worker_deadbeef")?;

        let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(0o600, mode, "secret file must be owner-read/write only");
        assert_eq!(
            "silent_critic_worker_deadbeef",
            std::fs::read_to_string(&path)?
        );
        Ok(())
    }

    #[test]
    fn reports_the_underlying_io_error() {
        let actual = write_secret_file(std::path::Path::new("/no/such/directory/secret"), b"x");
        assert!(actual.is_err());
    }
}
