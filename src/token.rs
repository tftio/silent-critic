//! Task-scoped bearer tokens gating the worker write surface.
//!
//! A token authenticates a caller as one of two roles: the orchestrator (no
//! token — the MCP session itself is the operator's, per T005's brief), or a
//! worker bound to exactly one task. The registry that validates a presented
//! token is a disposable, per-run sidecar — not the operator-owned
//! `binding.toml` [`crate::store`] writes — persisted as TOML under a `run/`
//! subdirectory of the plan's store directory (e.g.
//! `<plan_dir>/run/tokens.toml`), matching `binding.toml`'s TOML encoding
//! (ENG-014) but with a lifetime scoped to one run rather than to the plan:
//! T010 moves whatever of this belongs in the durable review artifact into
//! the plan through `tftio_planner`; nothing here is meant to survive that.
//!
//! Tokens are stored in this sidecar in plaintext, not hashed. The sidecar's
//! directory is operator-owned (the same trust boundary as `binding.toml`),
//! the file is disposable for one run, and the tokens it holds authorize
//! only a narrow write surface (worker notes and submissions) rather than
//! anything resembling a durable credential. Validation is a plain string
//! comparison (`BTreeMap` lookup): not constant-time, which would matter for
//! a network-facing secret but not for a local sidecar file compared against
//! a locally-spawned worker's own environment.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{PlanId, TaskId};

/// An opaque bearer token of the form `silent_critic_<role>_<uuid-simple>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token string (e.g. one presented by a caller).
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Borrow the raw token string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The role a validated token authenticates as.
///
/// A closed set: the orchestrator (which in practice never needs a minted
/// token — its scope is granted by the MCP session itself — but is
/// representable here so [`TokenRegistry`] has one type for every role it
/// might ever hold), and a worker bound to exactly one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The operator's own session; needs no token in practice.
    Orchestrator,
    /// A worker, authorized to act only on `task`.
    Worker {
        /// The one task this token's holder may act on.
        task: TaskId,
    },
}

impl Role {
    /// The token-prefix label for this role (`orchestrator` or `worker`).
    const fn label(&self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Worker { .. } => "worker",
        }
    }
}

/// One plan's minted tokens: a disposable, per-run sidecar record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRegistry {
    plan_id: PlanId,
    #[serde(default)]
    tokens: BTreeMap<String, Role>,
}

impl TokenRegistry {
    /// An empty registry scoped to `plan_id`.
    #[must_use]
    pub const fn new(plan_id: PlanId) -> Self {
        Self {
            plan_id,
            tokens: BTreeMap::new(),
        }
    }

    /// The plan this registry is scoped to.
    #[must_use]
    pub const fn plan_id(&self) -> &PlanId {
        &self.plan_id
    }

    /// Mint a fresh token for `role` and register it.
    ///
    /// `plan_id` names the plan the caller believes it is minting for; it
    /// must match this registry's own [`TokenRegistry::plan_id`].
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::PlanMismatch`] when `plan_id` does not match
    /// this registry's own scope. This is a recoverable, checked condition
    /// (not a `debug_assert!`) because a caller holding the wrong registry
    /// for a plan is exactly the mistake a colliding task id across two
    /// plans would otherwise turn into cross-plan token authentication.
    pub fn mint(&mut self, role: Role, plan_id: &PlanId) -> Result<Token, TokenError> {
        if &self.plan_id != plan_id {
            return Err(TokenError::PlanMismatch {
                expected: self.plan_id.clone(),
                given: plan_id.clone(),
            });
        }
        let raw = format!("silent_critic_{}_{}", role.label(), Uuid::new_v4().simple());
        let token = Token(raw);
        self.tokens.insert(token.0.clone(), role);
        Ok(token)
    }

    /// Validate a presented token, returning the role it authenticates as.
    #[must_use]
    pub fn validate(&self, token: &Token) -> Option<&Role> {
        self.tokens.get(&token.0)
    }

    /// Load a registry from its sidecar TOML file.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] when the file cannot be read or does not
    /// parse as a registry.
    pub fn load(path: &Path) -> Result<Self, TokenError> {
        let body = std::fs::read_to_string(path).map_err(io_error(path))?;
        toml::from_str(&body).map_err(|source| TokenError::Deserialize {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Save a registry to its sidecar TOML file, creating parent directories
    /// as needed.
    ///
    /// Written through the shared owner-only-permissions helper (fix round
    /// 2, finding #7): `tokens.toml` holds every worker token in
    /// cleartext, exactly the same kind of secret `mcp.json`
    /// (`src/dispatch.rs`) already restricted to `0o600` on Unix.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] when the parent directory cannot be created or
    /// the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), TokenError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        crate::secret_file::write_secret_file(path, self.to_toml_string().as_bytes())
            .map_err(io_error(path))
    }

    /// Render this registry as TOML.
    ///
    /// Every field a [`TokenRegistry`] can hold — a [`PlanId`], and a
    /// [`BTreeMap`] of token strings to [`Role`], itself built only from
    /// plain UTF-8 strings — is representable in TOML without exception, so
    /// this has no fallible counterpart: `unwrap_or_default` names a
    /// fallback this module does not expect ever to reach, rather than
    /// swallowing a real failure the way `.unwrap()` would panic on one.
    fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> TokenError + '_ {
    move |source| TokenError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Failures loading or saving a [`TokenRegistry`].
#[derive(Debug, Error)]
pub enum TokenError {
    /// A filesystem operation on the sidecar failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The sidecar could not be parsed as a token registry.
    #[error("parsing token registry {path}: {source}")]
    Deserialize {
        /// The sidecar path that failed to parse.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// A token was minted against a plan id other than the one this
    /// registry is scoped to.
    #[error("cannot mint a token for plan {given}: this registry is scoped to plan {expected}")]
    PlanMismatch {
        /// The plan this registry is actually scoped to.
        expected: PlanId,
        /// The plan the caller believed it was minting for.
        given: PlanId,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Role, Token, TokenError, TokenRegistry};
    use crate::model::{PlanId, TaskId};

    #[test]
    fn mint_produces_role_prefixed_tokens() -> Result<(), Box<dyn std::error::Error>> {
        let plan_id = PlanId::new("plan-1");
        let mut registry = TokenRegistry::new(plan_id.clone());

        let orchestrator_token = registry.mint(Role::Orchestrator, &plan_id)?;
        assert!(
            orchestrator_token
                .as_str()
                .starts_with("silent_critic_orchestrator_")
        );

        let worker_role = Role::Worker {
            task: TaskId::new("T001"),
        };
        let worker_token = registry.mint(worker_role, &plan_id)?;
        assert!(worker_token.as_str().starts_with("silent_critic_worker_"));
        assert_ne!(orchestrator_token, worker_token);
        Ok(())
    }

    #[test]
    fn mint_rejects_a_plan_id_the_registry_is_not_scoped_to() {
        let mut registry = TokenRegistry::new(PlanId::new("plan-a"));
        let other_plan = PlanId::new("plan-b");

        let actual = registry.mint(
            Role::Worker {
                task: TaskId::new("T001"),
            },
            &other_plan,
        );

        assert!(matches!(actual, Err(TokenError::PlanMismatch { .. })));
    }

    #[test]
    fn validate_returns_the_registered_role() -> Result<(), Box<dyn std::error::Error>> {
        let plan_id = PlanId::new("plan-1");
        let mut registry = TokenRegistry::new(plan_id.clone());
        let task = TaskId::new("T001");
        let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;

        assert_eq!(Some(&Role::Worker { task }), registry.validate(&token));
        Ok(())
    }

    #[test]
    fn validate_rejects_an_unknown_token() {
        let registry = TokenRegistry::new(PlanId::new("plan-1"));
        let unknown = Token::new("silent_critic_worker_does-not-exist");

        assert_eq!(None, registry.validate(&unknown));
    }

    #[test]
    fn token_displays_its_raw_value() {
        let token = Token::new("silent_critic_worker_abc123");
        assert_eq!("silent_critic_worker_abc123", token.to_string());
        assert_eq!("silent_critic_worker_abc123", token.as_str());
    }

    #[test]
    fn plan_id_accessor_returns_the_scoped_plan() {
        let plan_id = PlanId::new("plan-1");
        let registry = TokenRegistry::new(plan_id.clone());
        assert_eq!(&plan_id, registry.plan_id());
    }

    #[test]
    fn registry_round_trips_through_its_sidecar_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("run").join("tokens.toml");

        let plan_id = PlanId::new("plan-1");
        let mut registry = TokenRegistry::new(plan_id.clone());
        let orchestrator_token = registry.mint(Role::Orchestrator, &plan_id)?;
        let worker_role = Role::Worker {
            task: TaskId::new("T001"),
        };
        let worker_token = registry.mint(worker_role, &plan_id)?;
        registry.save(&path)?;

        let loaded = TokenRegistry::load(&path)?;
        assert_eq!(plan_id, loaded.plan_id);
        assert_eq!(
            Some(&Role::Orchestrator),
            loaded.validate(&orchestrator_token)
        );
        assert_eq!(
            Some(&Role::Worker {
                task: TaskId::new("T001")
            }),
            loaded.validate(&worker_token)
        );

        Ok(())
    }

    #[test]
    fn load_reports_missing_files() {
        let actual = TokenRegistry::load(std::path::Path::new("/no/such/tokens.toml"));
        assert!(matches!(actual, Err(TokenError::Io { .. })));
    }

    #[test]
    fn load_reports_malformed_files() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("tokens.toml");
        std::fs::write(&path, "not = [valid toml")?;

        let actual = TokenRegistry::load(&path);

        assert!(matches!(actual, Err(TokenError::Deserialize { .. })));
        Ok(())
    }

    #[test]
    fn save_with_no_parent_skips_create_dir_all() {
        // `Path::new("").parent()` is `None`: the empty path has no parent
        // component to create, so `save` must skip straight to the write
        // (which then fails, since "" is not a writable file path) rather
        // than panicking or erroring on `create_dir_all`.
        let registry = TokenRegistry::new(PlanId::new("plan-1"));
        let actual = registry.save(Path::new(""));
        assert!(matches!(actual, Err(TokenError::Io { .. })));
    }

    #[test]
    fn save_reports_create_dir_failures() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        // A plain file where the parent directory needs to go blocks
        // `create_dir_all`.
        let blocker = tmp.path().join("run");
        std::fs::write(&blocker, "not a directory")?;
        let path = blocker.join("tokens.toml");

        let registry = TokenRegistry::new(PlanId::new("plan-1"));
        let actual = registry.save(&path);

        assert!(matches!(actual, Err(TokenError::Io { .. })));
        Ok(())
    }
}
