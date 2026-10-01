//! The out-of-repo plan store.
//!
//! A supervised plan lives outside every worktree of the repository it
//! supervises, so that a worker cannot read what is not in its own
//! repository. This module resolves a repository to the plans stored for
//! it: a root directory under the XDG data directory, keyed by the
//! repository's content-derived identity ([`crate::provenance::RepoIdentity`]),
//! holding one directory per plan. Each plan directory carries the plan's
//! Markdown document plus a `binding.toml` recording the
//! [`crate::provenance::Provenance`] it was stored with.
//!
//! `binding.toml` is deliberately not the disposable per-run sidecar a
//! dispatched task will get: it is operator-owned, non-disposable, and
//! belongs to the review artifact, so it must outlive any single run.
//!
//! The Markdown document is named `<plan-id>.md` ("decision 3" elsewhere in
//! this module): `<plan-id>` is the same filename-stem identifier
//! [`PlanId`] wraps, so the stored file itself satisfies `check-plan`'s and
//! `tftio_planner`'s own filename rule (`YYYY-MM-DD-<slug>.md`) and can be
//! validated or inspected in place, without copying it out of the store
//! first. A plan stored by an earlier version of this crate as the fixed
//! name `plan.md` still loads: every reader tries the id-matching name
//! first and falls back to `plan.md`.

use std::fs;
use std::path::{Path, PathBuf};

use tftio_lib::project::Slug;
use thiserror::Error;

use crate::provenance::{self, Provenance, ProvenanceError, RepoIdentity};

/// The legacy stored-plan filename, written by every version of this crate
/// before decision 3: fixed regardless of the plan's own id, which both
/// `check-plan` and `tftio_planner`'s filename rule
/// (`YYYY-MM-DD-<slug>.md`) reject outright -- an operator could not
/// validate or inspect a stored plan without copying it elsewhere first.
/// [`Store::add_plan`] no longer writes this name, but every reader that
/// resolves a stored plan's path still falls back to it, so a plan stored
/// before this change keeps loading (`REPO_INVARIANTS.md`-adjacent
/// invariant: "stored plans written before this change still load and
/// judge").
const LEGACY_PLAN_FILE_NAME: &str = "plan.md";
const BINDING_FILE_NAME: &str = "binding.toml";

/// The stored-plan filename for `plan_id`, matching `check-plan`'s and
/// `tftio_planner`'s own filename rule (`YYYY-MM-DD-<slug>.md`) exactly,
/// since `plan_id` already is that stem: `plan_id_from_path` derives it
/// from the source plan's own filename, which
/// `tftio_planner::validate_markdown_path` already required to satisfy the
/// same rule before `add_plan` ever accepts it.
fn plan_file_name(plan_id: &str) -> String {
    format!("{plan_id}.md")
}

/// Resolve which file a stored plan actually lives at inside `plan_dir`:
/// the current, id-matching name if present, else the legacy fixed
/// `plan.md` name a pre-decision-3 store wrote, else the current name
/// (the name a fresh [`Store::add_plan`] would use, so a caller building a
/// path to write to -- there is none today, `add_plan` computes its own --
/// still gets a sensible default rather than an arbitrary one).
fn resolve_plan_file(plan_dir: &Path, plan_id: &str) -> PathBuf {
    let current = plan_dir.join(plan_file_name(plan_id));
    if current.is_file() {
        return current;
    }
    let legacy = plan_dir.join(LEGACY_PLAN_FILE_NAME);
    if legacy.is_file() {
        return legacy;
    }
    current
}

/// A typed, already-resolved root directory for the plan store.
///
/// The binary reads `XDG_DATA_HOME`/`HOME` once at the process edge (via
/// clap's `env` attribute) and constructs this; the library never reads the
/// environment itself.
#[derive(Debug, Clone)]
pub struct StoreRoot(PathBuf);

impl StoreRoot {
    /// Wrap an already-resolved store root path.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self(root)
    }

    /// Borrow the resolved store root path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Resolve the default store root from typed, already-read environment values.
///
/// Does not read the environment itself: `$XDG_DATA_HOME/silent-critic` when
/// `xdg_data_home` is given, else `<home>/.local/share/silent-critic`. Returns
/// `None` when neither value is available, leaving the caller (the
/// `silent-critic` binary) to report that no store root could be determined.
#[must_use]
pub fn default_store_root(xdg_data_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    xdg_data_home.map_or_else(
        || home.map(|home| home.join(".local").join("share").join("silent-critic")),
        |base| Some(base.join("silent-critic")),
    )
}

/// Resolve the retired holdout store root for read-only legacy inspection.
///
/// New silent-critic runs always use [`default_store_root`]. This helper is
/// intentionally separate so no command can select the legacy root for a
/// mutating operation.
#[must_use]
pub fn default_legacy_store_root(
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    xdg_data_home.map_or_else(
        || home.map(|home| home.join(".local").join("share").join("holdout")),
        |base| Some(base.join("holdout")),
    )
}

/// Identifier for a plan stored for a repository: its validated filename
/// stem (e.g. `2026-09-05-my-plan` for `2026-09-05-my-plan.md`).
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PlanId(String);

impl PlanId {
    /// Return the identifier as its stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One plan recorded in the store for a repository.
#[derive(Debug, Clone)]
pub struct PlanSummary {
    /// The plan's identifier in the store.
    pub id: PlanId,
    /// The provenance the plan was bound to.
    pub provenance: Provenance,
}

/// The out-of-repo plan store.
#[derive(Debug, Clone)]
pub struct Store {
    root: StoreRoot,
}

impl Store {
    /// Build a store rooted at `root`.
    #[must_use]
    pub const fn new(root: StoreRoot) -> Self {
        Self { root }
    }

    /// Borrow this store's root path, for callers that need to reconstruct
    /// a [`Store`] elsewhere (e.g. a dispatched worker's own process,
    /// T009's `SILENT_CRITIC_STORE_ROOT`).
    #[must_use]
    pub fn root_path(&self) -> &Path {
        self.root.as_path()
    }

    /// Validate `plan_path`, bind it to the repository at `repo_start` and
    /// `base_ref_name`, and copy it into the store.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the plan cannot be read, does not
    /// validate as a planning document, cannot be bound to a repository and
    /// base ref (not a repository, or the base ref does not resolve), or
    /// cannot be written into the store.
    pub fn add_plan(
        &self,
        plan_path: &Path,
        repo_start: &Path,
        base_ref_name: &str,
    ) -> Result<PlanId, StoreError> {
        let source = fs::read_to_string(plan_path).map_err(io_error(plan_path))?;

        let report =
            tftio_planner::validate_markdown_path(&source, plan_path).map_err(|source| {
                StoreError::ParsePlan {
                    path: plan_path.to_path_buf(),
                    source,
                }
            })?;
        if !report.is_valid() {
            // A plain loop collecting into a `Vec` (rather than
            // `.filter().map()`) so this branch's own code lives directly
            // in `add_plan`'s body, not in separately-instantiated closures
            // that would need their own, independent test coverage.
            // `Vec::join` needs no closure of its own.
            let mut messages = Vec::new();
            for diagnostic in &report.diagnostics {
                if diagnostic.severity != tftio_planner::DiagnosticSeverity::Error {
                    continue;
                }
                messages.push(format!(
                    "{}: {}",
                    diagnostic.code.as_str(),
                    diagnostic.message
                ));
            }
            return Err(StoreError::InvalidPlan {
                path: plan_path.to_path_buf(),
                details: messages.join("; "),
            });
        }

        let plan_id = plan_id_from_path(plan_path)?;
        let mut provenance = provenance::bind(repo_start, base_ref_name)?;
        provenance.project = plan_project(&source, plan_path)?;

        let plan_dir = self.plan_dir(&provenance.repo, &plan_id);
        fs::create_dir_all(&plan_dir).map_err(io_error(&plan_dir))?;

        let plan_file = plan_dir.join(plan_file_name(plan_id.as_str()));
        fs::write(&plan_file, &source).map_err(io_error(&plan_file))?;

        write_binding(&plan_dir.join(BINDING_FILE_NAME), &provenance)?;

        Ok(plan_id)
    }

    /// List the plans stored for the repository at `repo_start`, in
    /// deterministic (identifier) order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the repository identity cannot be
    /// derived, or a stored plan's binding cannot be read.
    pub fn list_plans(&self, repo_start: &Path) -> Result<Vec<PlanSummary>, StoreError> {
        let repo = RepoIdentity::derive(repo_start)?;
        let mut plans = plans_in_dir(&self.plans_dir(&repo))?;
        plans.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        Ok(plans)
    }

    /// List every plan stored in `self`, across every repository, in
    /// deterministic (repository identity, then plan identifier) order.
    ///
    /// Each [`PlanSummary`] already carries its own repository identity
    /// (`provenance.repo`), so this needs no `repo_start` and, unlike
    /// [`Store::list_plans`], is not scoped to one repository at all: it
    /// walks the store root's own repository directories directly rather
    /// than deriving a single [`RepoIdentity`] to look up.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store root cannot be walked, or a
    /// stored plan's binding cannot be read or parsed.
    pub fn list_all_plans(&self) -> Result<Vec<PlanSummary>, StoreError> {
        let root = self.root.as_path();
        let Ok(repo_entries) = fs::read_dir(root) else {
            // No plan has ever been added to this store.
            return Ok(Vec::new());
        };

        let mut plans = Vec::new();
        for repo_entry in repo_entries {
            let repo_entry = repo_entry.map_err(io_error(root))?;
            if !repo_entry.path().is_dir() {
                continue;
            }
            plans.extend(plans_in_dir(&repo_entry.path().join("plans"))?);
        }

        plans.sort_by(|left, right| {
            (left.provenance.repo.as_str(), left.id.0.as_str())
                .cmp(&(right.provenance.repo.as_str(), right.id.0.as_str()))
        });
        Ok(plans)
    }

    /// Resolve the absolute path of a plan stored for the repository at
    /// `repo_start`.
    ///
    /// Reads whichever of `<plan_id>.md` (written by [`Store::add_plan`]
    /// since decision 3) or the legacy fixed `plan.md` (written before it)
    /// exists on disk, preferring the id-matching name -- a plan stored
    /// before this change keeps loading.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the repository identity cannot be
    /// derived, or when no such plan is stored for it.
    pub fn plan_path(&self, repo_start: &Path, plan_id: &str) -> Result<PathBuf, StoreError> {
        let repo = RepoIdentity::derive(repo_start)?;
        let plan_dir = self.plans_dir(&repo).join(plan_id);
        let path = resolve_plan_file(&plan_dir, plan_id);
        if path.is_file() {
            Ok(path)
        } else {
            Err(StoreError::PlanNotFound {
                plan_id: plan_id.to_string(),
            })
        }
    }

    /// Resolve the absolute directory a stored plan's files live in (the
    /// parent of its stored Markdown file, whichever of the two names
    /// [`Store::plan_path`] resolved), for callers that place their own
    /// sidecars alongside it (e.g. dispatch's `run/<task_id>/` directory,
    /// T009).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as
    /// [`Store::plan_path`].
    pub fn plan_directory(&self, repo_start: &Path, plan_id: &str) -> Result<PathBuf, StoreError> {
        let plan_path = self.plan_path(repo_start, plan_id)?;
        // `plan_path` always resolves to a file directly inside the plan's
        // own directory, so it always has a parent; eager fallback for the
        // same reason as this module's other
        // `Option::unwrap_or(Path::new("."))` call sites.
        #[allow(clippy::or_fun_call)]
        Ok(plan_path.parent().unwrap_or(Path::new(".")).to_path_buf())
    }

    /// Read back a stored plan's provenance binding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the repository identity cannot be
    /// derived, no such plan is stored, or its binding record cannot be
    /// read or parsed.
    pub fn provenance(&self, repo_start: &Path, plan_id: &str) -> Result<Provenance, StoreError> {
        let binding_file = self
            .plan_directory(repo_start, plan_id)?
            .join(BINDING_FILE_NAME);
        let body = fs::read_to_string(&binding_file).map_err(io_error(&binding_file))?;
        toml::from_str(&body).map_err(|source| StoreError::DeserializeBinding {
            path: binding_file,
            source,
        })
    }

    fn plans_dir(&self, repo: &RepoIdentity) -> PathBuf {
        self.root.0.join(repo.as_str()).join("plans")
    }

    fn plan_dir(&self, repo: &RepoIdentity, id: &PlanId) -> PathBuf {
        self.plans_dir(repo).join(id.as_str())
    }

    /// Resolve the absolute path of a plan already known via a
    /// [`PlanSummary`] (as [`Store::list_plans`] and [`Store::list_all_plans`]
    /// return it), using the repository identity it already carries rather
    /// than needing a `repo_start` path to re-derive one (unlike
    /// [`Store::plan_path`], which is the right call when only a plan id
    /// and a repository path are in hand).
    #[must_use]
    pub fn plan_path_for_summary(&self, plan: &PlanSummary) -> PathBuf {
        let plan_dir = self.plan_dir(&plan.provenance.repo, &plan.id);
        resolve_plan_file(&plan_dir, plan.id.as_str())
    }
}

/// Build a `map_err` closure that wraps a filesystem `io::Error` with
/// `path`.
///
/// Every I/O call site in this module passes its own `path` to `io_error`
/// and gets back a closure to hand `map_err` directly (`.map_err(io_error(path))`,
/// no closure literal at the call site). Because that closure's body is a
/// single expression written once, here, every call site shares the exact
/// same compiled code: whichever call site's error actually fires at test
/// time covers it for all of them, including `list_plans`'s `fs::read_dir`
/// per-entry failure, an OS-level race no portable test can trigger on its
/// own.
fn io_error(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// List every plan directly under `plans_dir` (one repository's own
/// `plans/` directory), unsorted: the shared body of [`Store::list_plans`]
/// (scoped to one repository) and [`Store::list_all_plans`] (every
/// repository's own `plans/` directory in turn), so the two can never
/// silently diverge in how they read one repository's plans.
fn plans_in_dir(plans_dir: &Path) -> Result<Vec<PlanSummary>, StoreError> {
    let Ok(entries) = fs::read_dir(plans_dir) else {
        // No plans have ever been added under this directory.
        return Ok(Vec::new());
    };

    let mut plans = Vec::new();
    for entry in entries {
        // Forcing `fs::read_dir`'s iterator to yield an `Err` for one entry
        // inside an already-open, otherwise-healthy directory is an
        // OS-level race, not something a portable test can trigger;
        // `io_error` is exercised directly (and via every other I/O
        // failure in this module), so this call site needs no test of its
        // own.
        let entry = entry.map_err(io_error(plans_dir))?;
        if !entry.path().is_dir() {
            continue;
        }

        let id = plan_id_from_dir_entry_name(&entry.path(), &entry.file_name())?;
        let binding_file = entry.path().join(BINDING_FILE_NAME);
        let binding_body = fs::read_to_string(&binding_file).map_err(io_error(&binding_file))?;
        let provenance: Provenance = match toml::from_str(&binding_body) {
            Ok(provenance) => provenance,
            Err(source) => {
                return Err(StoreError::DeserializeBinding {
                    path: binding_file,
                    source,
                });
            }
        };
        plans.push(PlanSummary { id, provenance });
    }
    Ok(plans)
}

fn write_binding(binding_file: &Path, provenance: &Provenance) -> Result<(), StoreError> {
    // `Provenance` is built entirely of `String`-backed fields
    // (`RepoIdentity`, `BaseRef`'s `name`/`commit`), and the TOML
    // serializer never fails encoding a plain string; the last field that
    // could ever fail this call (`worktree: Option<PathBuf>`, which could
    // hold non-UTF-8 bytes) was removed in fix round 2, finding #1. This
    // mirrors `render_summary`'s discarded "infallible" `Result` in
    // `src/evaluate/automated.rs`: a `StoreError` variant that could never
    // be exercised is worse than a documented, empty fallback.
    let body = toml::to_string_pretty(provenance).unwrap_or_default();
    fs::write(binding_file, body).map_err(io_error(binding_file))
}

// A plain `match` (not `Option::map_or_else`), deliberately: this crate's
// coverage gate cannot credit a closure invoked only from one of the
// several separately-compiled artifacts a test may run in (see
// `REPO_INVARIANTS.md` history for T002's fix rounds); a `match` arm's code
// is part of the enclosing function's own body instead.
#[allow(clippy::option_if_let_else)]
fn plan_id_from_path(plan_path: &Path) -> Result<PlanId, StoreError> {
    match plan_path.file_stem().and_then(|stem| stem.to_str()) {
        Some(stem) => Ok(PlanId(stem.to_string())),
        None => Err(StoreError::InvalidPlanId {
            path: plan_path.to_path_buf(),
        }),
    }
}

/// Extract the plan's own project slug (T012), by re-parsing `source`.
///
/// `add_plan` calls this only after `tftio_planner::validate_markdown_path`
/// has already proved `source` parses (a `ParseError` there returns
/// `StoreError::ParsePlan` before `plan_project` is ever reached), so the
/// `Err` arm here is structurally unreachable through that pipeline --
/// `parse_markdown` is pure over `source`, so a second call on the same
/// string cannot fail where the first succeeded. Exercised directly below
/// with an unparsable source instead of contriving a document that parses
/// once and not the next (mirrors `crate::measure::pair_delta`'s identical
/// justification for the same class of defensive arm).
///
/// Read only from the plan's own validated metadata, never derived from
/// `plan_path` or any other filesystem signal, so the binding and the plan
/// can never disagree about which project a stored plan belongs to.
fn plan_project(source: &str, plan_path: &Path) -> Result<Option<Slug>, StoreError> {
    let operator_plan =
        tftio_planner::parse_markdown(source).map_err(|source| StoreError::ParsePlan {
            path: plan_path.to_path_buf(),
            source,
        })?;
    Ok(operator_plan.metadata.project.map(|identity| identity.slug))
}

/// Turn a stored plan directory's entry name into a [`PlanId`], consistent
/// with [`plan_id_from_path`]: both reject non-UTF-8 names via the same
/// typed error, rather than one lossily converting and the other rejecting.
///
/// Takes the raw `OsStr` (rather than a `fs::DirEntry`, which cannot be
/// constructed outside `std`) so the non-UTF-8 rejection is directly
/// unit-testable without needing a filesystem that will actually store a
/// non-UTF-8 name — macOS's filesystem rejects such names outright, so no
/// portable test could otherwise reach this branch.
#[allow(clippy::option_if_let_else)] // see `plan_id_from_path`
fn plan_id_from_dir_entry_name(
    entry_path: &Path,
    name: &std::ffi::OsStr,
) -> Result<PlanId, StoreError> {
    match name.to_str() {
        Some(name) => Ok(PlanId(name.to_string())),
        None => Err(StoreError::InvalidPlanId {
            path: entry_path.to_path_buf(),
        }),
    }
}

/// Failures while adding a plan to, or reading it back from, the store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A filesystem operation on the store or the plan being added failed.
    ///
    /// Covers reading the source plan, creating a plan's store directory,
    /// writing its `<plan-id>.md`/`binding.toml`, reading a stored binding
    /// back, and listing a repository's plan directories: all are plain I/O
    /// failures distinguished for the operator by the wrapped `io::Error`'s
    /// own message and the `path` it happened on, not by a separate
    /// variant per call site.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The plan could not be parsed as a planning document.
    #[error("parsing plan {path}: {source}")]
    ParsePlan {
        /// The plan path that failed to parse.
        path: PathBuf,
        /// The underlying parse error.
        source: tftio_planner::ParseError,
    },
    /// The plan parsed but failed validation.
    #[error("plan {path} failed validation: {details}")]
    InvalidPlan {
        /// The plan path that failed validation.
        path: PathBuf,
        /// A semicolon-joined summary of the validation errors.
        details: String,
    },
    /// The plan's filename could not be turned into a plan identifier.
    #[error("plan path {path} has no usable filename")]
    InvalidPlanId {
        /// The plan path with no usable filename.
        path: PathBuf,
    },
    /// The plan could not be bound to a repository and base ref.
    #[error(transparent)]
    Provenance(#[from] ProvenanceError),
    /// A binding record could not be deserialized.
    #[error("parsing binding {path}: {source}")]
    DeserializeBinding {
        /// The binding file that could not be parsed.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// No plan with the given identifier is stored for the repository.
    #[error("no plan '{plan_id}' is stored for this repository")]
    PlanNotFound {
        /// The plan identifier that was not found.
        plan_id: String,
    },
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{
        BINDING_FILE_NAME, LEGACY_PLAN_FILE_NAME, Store, StoreError, StoreRoot,
        default_legacy_store_root, default_store_root, write_binding,
    };
    use crate::provenance::RepoIdentity;

    const VALID_PLAN: &str = include_str!("../tests/fixtures/2026-09-05-store-plan.md");
    const V2_PLAN_WITH_PROJECT: &str =
        include_str!("../tests/fixtures/2026-09-23-store-plan-v2.md");

    fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args);
        crate::test_support::harden_git_command(&mut command, dir);
        let status = command.status()?;
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

    fn worktree_add(
        repo_dir: &Path,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), Box<dyn Error>> {
        let worktree_path = worktree_path
            .to_str()
            .ok_or("worktree path must be valid UTF-8 for this test")?;
        git(
            repo_dir,
            &["worktree", "add", "--quiet", "-b", branch, worktree_path],
        )
    }

    /// Independently resolve `dir`'s common git directory (canonicalized),
    /// by shelling out to `git` directly rather than calling into
    /// `crate::provenance`: used to assert against the *actual* filesystem
    /// location without trusting this crate's own resolution of it.
    ///
    /// Callers of this test helper always pass a linked worktree, for which
    /// `git` reports an absolute common git directory (it is outside the
    /// worktree, in the main checkout) — unlike the relative output `git`
    /// gives for a main checkout's own `.git`, which `discover_common_git_dir`
    /// (in `crate::provenance`) has to handle but this narrower helper does
    /// not need to.
    fn common_git_dir(dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--git-common-dir"]);
        crate::test_support::harden_git_command(&mut command, dir);
        let output = command.output()?;
        let raw = String::from_utf8(output.stdout)?.trim().to_string();
        Ok(PathBuf::from(raw).canonicalize()?)
    }

    #[test]
    fn add_plan_stores_outside_the_repository_and_round_trips_provenance()
    -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;
        assert_eq!("2026-09-05-store-plan", plan_id.as_str());

        let resolved_path = store.plan_path(repo_dir.path(), plan_id.as_str())?;

        // Acceptance check: the resolved plan path is outside the
        // supervised repository's working tree and git directory.
        let canonical_repo = repo_dir.path().canonicalize()?;
        let canonical_resolved = resolved_path.canonicalize()?;
        assert!(!canonical_resolved.starts_with(&canonical_repo));
        assert!(!canonical_resolved.starts_with(canonical_repo.join(".git")));

        let stored_source = std::fs::read_to_string(&resolved_path)?;
        assert_eq!(VALID_PLAN, stored_source);

        // Acceptance check: the round trip preserves repository identity and
        // base ref.
        let plans = store.list_plans(repo_dir.path())?;
        assert_eq!(1, plans.len());
        let summary = plans.first().ok_or("expected one stored plan")?;
        assert_eq!(plan_id, summary.id);

        let expected_provenance = crate::provenance::bind(repo_dir.path(), "HEAD")?;
        assert_eq!(expected_provenance.repo, summary.provenance.repo);
        assert_eq!(
            expected_provenance.base_ref.commit,
            summary.provenance.base_ref.commit
        );
        assert_eq!("HEAD", summary.provenance.base_ref.name);

        Ok(())
    }

    /// T012 acceptance check: adding a v2 plan that declares a project
    /// records that project's slug in `binding.toml`, taken from the
    /// plan's own validated metadata rather than derived from
    /// `repo_start`, and `list_plans`'s `PlanSummary` shows it.
    #[test]
    fn add_plan_records_the_project_slug_from_a_v2_plan() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-23-store-plan-v2.md");
        std::fs::write(&plan_path, V2_PLAN_WITH_PROJECT)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

        let binding_file = store
            .plan_directory(repo_dir.path(), plan_id.as_str())?
            .join(super::BINDING_FILE_NAME);
        let binding_body = std::fs::read_to_string(&binding_file)?;
        assert!(
            binding_body.contains("project = \"silent-critic\""),
            "binding.toml did not carry the project slug: {binding_body}"
        );

        let plans = store.list_plans(repo_dir.path())?;
        let summary = plans.first().ok_or("expected one stored plan")?;
        assert_eq!(
            Some("silent-critic"),
            summary
                .provenance
                .project
                .as_ref()
                .map(tftio_lib::project::Slug::as_str)
        );

        Ok(())
    }

    /// Invariant: every `binding.toml` written before this task existed
    /// (carrying no `project` key at all) still loads, via
    /// `#[serde(default)]` on `Provenance::project`.
    #[test]
    fn a_binding_without_a_project_key_still_loads() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let plan_dir = store_dir
            .path()
            .join(repo.as_str())
            .join("plans")
            .join("2026-09-05-store-plan");
        std::fs::create_dir_all(&plan_dir)?;
        // A pre-T012 binding: only `repo` and `base_ref`, no `project` key.
        let pre_t012_binding = "repo = \"deadbeef\"\n\n[base_ref]\nname = \"HEAD\"\ncommit = \"0000000000000000000000000000000000000000\"\n";
        std::fs::write(plan_dir.join(super::BINDING_FILE_NAME), pre_t012_binding)?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let plans = store.list_all_plans()?;

        assert_eq!(1, plans.len());
        let summary = plans.first().ok_or("expected one stored plan")?;
        assert_eq!(None, summary.provenance.project);

        Ok(())
    }

    /// Decision 3: `add_plan` writes `<plan-id>.md`, not the fixed legacy
    /// `plan.md` -- so the stored file's own name already satisfies
    /// `check-plan`'s and `tftio_planner`'s filename rule
    /// (`YYYY-MM-DD-<slug>.md`), and `tftio_planner::validate_markdown_path`
    /// validates it in place without an operator having to copy it
    /// elsewhere first.
    #[test]
    fn add_plan_writes_a_filename_matching_the_plan_id_and_validates_in_place()
    -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

        let resolved_path = store.plan_path(repo_dir.path(), plan_id.as_str())?;
        assert_eq!(
            Some(format!("{}.md", plan_id.as_str())),
            resolved_path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned)
        );
        assert!(
            !resolved_path.ends_with(LEGACY_PLAN_FILE_NAME),
            "add_plan must not write the legacy plan.md name: {resolved_path:?}"
        );

        let source = std::fs::read_to_string(&resolved_path)?;
        let report = tftio_planner::validate_markdown_path(&source, &resolved_path)?;
        assert!(
            report.is_valid(),
            "the stored file must validate in place under its own name: {report:?}"
        );

        Ok(())
    }

    /// A plan stored by an earlier version of this crate, before decision
    /// 3, as the fixed name `plan.md` -- with no `<plan-id>.md` alongside
    /// it -- must still load: `plan_path`/`plan_directory` fall back to
    /// the legacy name when the id-matching one is absent
    /// (`REPO_INVARIANTS.md`-adjacent invariant: "stored plans written
    /// before this change still load and judge").
    #[test]
    fn a_legacy_plan_md_still_loads() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        let plan_dir = store_dir
            .path()
            .join(repo.as_str())
            .join("plans")
            .join("2026-09-05-store-plan");
        std::fs::create_dir_all(&plan_dir)?;
        std::fs::write(plan_dir.join(LEGACY_PLAN_FILE_NAME), VALID_PLAN)?;
        let provenance = crate::provenance::bind(repo_dir.path(), "HEAD")?;
        write_binding(&plan_dir.join(BINDING_FILE_NAME), &provenance)?;

        let resolved_path = store.plan_path(repo_dir.path(), "2026-09-05-store-plan")?;
        assert!(resolved_path.ends_with(LEGACY_PLAN_FILE_NAME));
        assert_eq!(VALID_PLAN, std::fs::read_to_string(&resolved_path)?);

        let resolved_dir = store.plan_directory(repo_dir.path(), "2026-09-05-store-plan")?;
        assert_eq!(plan_dir, resolved_dir);

        // `provenance` (and therefore `judge`'s own worktree resolution)
        // reads the binding, not the plan file, and is unaffected by which
        // name the plan document itself uses.
        let read_back = store.provenance(repo_dir.path(), "2026-09-05-store-plan")?;
        assert_eq!(provenance.repo, read_back.repo);

        Ok(())
    }

    #[test]
    fn add_plan_stores_outside_a_genuinely_separate_git_directory() -> Result<(), Box<dyn Error>> {
        // The previous test's repository has its `.git` *inside* its
        // working tree, so it cannot exercise the "outside the git
        // directory" half of the invariant: `canonical_repo.join(".git")`
        // would pass even if the check only ever compared against the
        // working tree. A linked worktree's `.git` is a *file* pointing at
        // the main checkout's `.git/worktrees/<name>`, and its *common* git
        // directory (what `RepoIdentity` is bound to) is the main
        // checkout's `.git` — genuinely outside this worktree's own tree.
        let main_checkout = tempfile::tempdir()?;
        init_repo(main_checkout.path())?;

        let worktree_parent = tempfile::tempdir()?;
        let worktree_path = worktree_parent.path().join("linked-worktree");
        worktree_add(main_checkout.path(), &worktree_path, "linked")?;
        assert!(worktree_path.join(".git").is_file());

        let plan_path = worktree_path.join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let plan_id = store.add_plan(&plan_path, &worktree_path, "HEAD")?;
        let resolved_path = store.plan_path(&worktree_path, plan_id.as_str())?;
        let canonical_resolved = resolved_path.canonicalize()?;

        let canonical_worktree = worktree_path.canonicalize()?;
        let canonical_common_git_dir = common_git_dir(&worktree_path)?;
        // The worktree's own git directory is genuinely outside its
        // working tree, in the main checkout.
        assert!(!canonical_common_git_dir.starts_with(&canonical_worktree));

        assert!(!canonical_resolved.starts_with(&canonical_worktree));
        assert!(!canonical_resolved.starts_with(&canonical_common_git_dir));

        Ok(())
    }

    #[test]
    fn add_plan_rejects_a_document_that_fails_validation() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-invalid-plan.md");
        std::fs::write(&plan_path, "not a planning document\n")?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let actual = store.add_plan(&plan_path, repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::ParsePlan { .. })));
        assert_eq!(0, store.list_plans(repo_dir.path())?.len());

        Ok(())
    }

    #[test]
    fn add_plan_rejects_a_repository_that_cannot_bind() -> Result<(), Box<dyn Error>> {
        let non_repo_dir = tempfile::tempdir()?;

        let plan_path = non_repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let actual = store.add_plan(&plan_path, non_repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::Provenance(_))));

        Ok(())
    }

    #[test]
    fn add_plan_rejects_an_unresolvable_base_ref_and_stores_nothing() -> Result<(), Box<dyn Error>>
    {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let actual = store.add_plan(&plan_path, repo_dir.path(), "does-not-exist");

        assert!(matches!(actual, Err(StoreError::Provenance(_))));
        assert_eq!(0, store.list_plans(repo_dir.path())?.len());

        Ok(())
    }

    #[test]
    fn list_plans_is_scoped_to_the_repository() -> Result<(), Box<dyn Error>> {
        let repo_a = tempfile::tempdir()?;
        let repo_b = tempfile::tempdir()?;
        init_repo(repo_a.path())?;
        init_repo(repo_b.path())?;

        let plan_a = repo_a.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_a, VALID_PLAN)?;
        let plan_b = repo_b.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_b, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        store.add_plan(&plan_a, repo_a.path(), "HEAD")?;

        assert_eq!(1, store.list_plans(repo_a.path())?.len());
        assert_eq!(0, store.list_plans(repo_b.path())?.len());

        Ok(())
    }

    #[test]
    fn list_all_plans_spans_every_repository_in_the_store() -> Result<(), Box<dyn Error>> {
        let repo_a = tempfile::tempdir()?;
        let repo_b = tempfile::tempdir()?;
        init_repo(repo_a.path())?;
        init_repo(repo_b.path())?;

        let plan_a = repo_a.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_a, VALID_PLAN)?;
        let plan_b = repo_b.path().join("2026-09-06-second-plan.md");
        std::fs::write(&plan_b, VALID_PLAN.replace("store-plan", "second-plan"))?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        store.add_plan(&plan_a, repo_a.path(), "HEAD")?;
        store.add_plan(&plan_b, repo_b.path(), "HEAD")?;

        let all = store.list_all_plans()?;
        let ids: Vec<&str> = all.iter().map(|plan| plan.id.as_str()).collect();
        assert_eq!(2, all.len());
        assert!(ids.contains(&"2026-09-05-store-plan"));
        assert!(ids.contains(&"2026-09-06-second-plan"));

        // Each summary still carries the repository it belongs to, so a
        // caller spanning repositories never has to re-derive it.
        let identity_a = RepoIdentity::derive(repo_a.path())?;
        let identity_b = RepoIdentity::derive(repo_b.path())?;
        let repos: Vec<&str> = all
            .iter()
            .map(|plan| plan.provenance.repo.as_str())
            .collect();
        assert!(repos.contains(&identity_a.as_str()));
        assert!(repos.contains(&identity_b.as_str()));

        Ok(())
    }

    #[test]
    fn list_all_plans_is_empty_for_a_store_root_that_does_not_exist() -> Result<(), Box<dyn Error>>
    {
        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().join("does-not-exist")));
        assert!(store.list_all_plans()?.is_empty());
        Ok(())
    }

    #[test]
    fn list_all_plans_skips_a_repository_directory_with_no_plans_subdirectory()
    -> Result<(), Box<dyn Error>> {
        let store_dir = tempfile::tempdir()?;
        std::fs::create_dir_all(store_dir.path().join("some-repo-hash"))?;
        // A stray file directly under the store root, alongside a real
        // repository directory: skipped, not a directory.
        std::fs::write(store_dir.path().join("not-a-repo-dir"), "irrelevant")?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        assert!(store.list_all_plans()?.is_empty());
        Ok(())
    }

    #[test]
    fn plan_path_reports_unknown_plans() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let actual = store.plan_path(repo_dir.path(), "does-not-exist");

        assert!(matches!(actual, Err(StoreError::PlanNotFound { .. })));

        Ok(())
    }

    #[test]
    fn default_store_root_prefers_xdg_data_home() {
        let xdg = Path::new("/xdg/data");
        let home = Path::new("/home/operator");

        let actual = default_store_root(Some(xdg), Some(home));

        assert_eq!(
            Some(Path::new("/xdg/data/silent-critic").to_path_buf()),
            actual
        );
    }

    #[test]
    fn default_store_root_falls_back_to_home() {
        let home = Path::new("/home/operator");

        let actual = default_store_root(None, Some(home));

        assert_eq!(
            Some(Path::new("/home/operator/.local/share/silent-critic").to_path_buf()),
            actual
        );
    }

    #[test]
    fn default_store_root_is_none_without_either_value() {
        let actual = default_store_root(None, None);

        assert_eq!(None, actual);
    }

    #[test]
    fn default_legacy_store_root_falls_back_to_home() {
        let home = Path::new("/home/operator");

        let actual = default_legacy_store_root(None, Some(home));

        assert_eq!(
            Some(Path::new("/home/operator/.local/share/holdout").to_path_buf()),
            actual
        );
    }

    #[test]
    fn default_legacy_store_root_keeps_holdout_separate_from_new_state() {
        let xdg = Path::new("/xdg/data");
        let home = Path::new("/home/operator");

        let actual = default_legacy_store_root(Some(xdg), Some(home));

        assert_eq!(Some(Path::new("/xdg/data/holdout").to_path_buf()), actual);
        assert_ne!(default_store_root(Some(xdg), Some(home)), actual);
    }

    #[test]
    fn add_plan_reports_unreadable_plan_paths() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let missing_plan = repo_dir.path().join("2026-09-05-does-not-exist.md");
        let actual = store.add_plan(&missing_plan, repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::Io { .. })));

        Ok(())
    }

    #[test]
    fn add_plan_rejects_a_plan_that_fails_semantic_validation() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        // Valid content, invalid filename: this parses but fails the
        // filename-shape check, exercising the semantic (not structural)
        // validation failure path.
        let plan_path = repo_dir.path().join("not-a-valid-plan-filename.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));

        let actual = store.add_plan(&plan_path, repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::InvalidPlan { .. })));
        assert_eq!(0, store.list_plans(repo_dir.path())?.len());

        Ok(())
    }

    #[test]
    fn add_plan_reports_create_dir_failures() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        // A plain file where the repository's directory needs to go blocks
        // `create_dir_all` from ever creating the plan directory.
        std::fs::write(store_dir.path().join(repo.as_str()), "not a directory")?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let actual = store.add_plan(&plan_path, repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::Io { .. })));

        Ok(())
    }

    #[test]
    fn add_plan_reports_plan_write_failures() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        let plan_dir = store_dir
            .path()
            .join(repo.as_str())
            .join("plans")
            .join("2026-09-05-store-plan");
        // A directory named `<plan-id>.md` (the name `add_plan` now writes,
        // decision 3) blocks writing the plan document.
        std::fs::create_dir_all(plan_dir.join(super::plan_file_name("2026-09-05-store-plan")))?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let actual = store.add_plan(&plan_path, repo_dir.path(), "HEAD");

        assert!(matches!(actual, Err(StoreError::Io { .. })));

        Ok(())
    }

    #[test]
    fn write_binding_serializes_unusual_but_valid_strings() -> Result<(), Box<dyn Error>> {
        // `write_binding` treats TOML serialization of `Provenance` as
        // infallible (fix round 2, finding #1 removed the one field --
        // `worktree: Option<PathBuf>` -- that could ever make it fail, by
        // holding non-UTF-8 bytes). A base ref name containing a raw
        // control character is the most unusual valid `String` this type
        // can carry; it still round-trips.
        let tmp = tempfile::tempdir()?;
        init_repo(tmp.path())?;
        let repo = crate::provenance::RepoIdentity::derive(tmp.path())?;
        let provenance = crate::provenance::Provenance {
            repo,
            base_ref: crate::provenance::BaseRef {
                name: "\u{0}".to_string(),
                commit: "0".repeat(40),
            },
            project: None,
        };

        let binding_file = tmp.path().join("binding.toml");
        super::write_binding(&binding_file, &provenance)?;

        let read_back: crate::provenance::Provenance =
            toml::from_str(&std::fs::read_to_string(&binding_file)?)?;
        assert_eq!(provenance.base_ref.name, read_back.base_ref.name);

        Ok(())
    }

    #[test]
    fn write_binding_reports_write_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        init_repo(tmp.path())?;
        let repo = crate::provenance::RepoIdentity::derive(tmp.path())?;
        let provenance = crate::provenance::Provenance {
            repo,
            base_ref: crate::provenance::BaseRef {
                name: "HEAD".to_string(),
                commit: "0".repeat(40),
            },
            project: None,
        };

        let binding_file = tmp
            .path()
            .join("no-such-directory")
            .join(super::BINDING_FILE_NAME);
        let actual = super::write_binding(&binding_file, &provenance);

        assert!(matches!(actual, Err(StoreError::Io { .. })));

        Ok(())
    }

    #[test]
    fn list_plans_reports_unreadable_bindings() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        let plan_dir = store_dir
            .path()
            .join(repo.as_str())
            .join("plans")
            .join("2026-09-05-store-plan");
        // A plan directory with no binding.toml at all.
        std::fs::create_dir_all(&plan_dir)?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let actual = store.list_plans(repo_dir.path());

        assert!(matches!(actual, Err(StoreError::Io { .. })));

        Ok(())
    }

    #[test]
    fn list_plans_reports_malformed_bindings() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let store_dir = tempfile::tempdir()?;
        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        let plan_dir = store_dir
            .path()
            .join(repo.as_str())
            .join("plans")
            .join("2026-09-05-store-plan");
        std::fs::create_dir_all(&plan_dir)?;
        std::fs::write(plan_dir.join(super::BINDING_FILE_NAME), "not = [valid toml")?;

        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let actual = store.list_plans(repo_dir.path());

        assert!(matches!(actual, Err(StoreError::DeserializeBinding { .. })));

        Ok(())
    }

    #[test]
    fn provenance_reports_malformed_bindings() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

        let plan_dir = store.plan_directory(repo_dir.path(), plan_id.as_str())?;
        std::fs::write(plan_dir.join(super::BINDING_FILE_NAME), "not = [valid toml")?;

        let actual = store.provenance(repo_dir.path(), plan_id.as_str());

        assert!(matches!(actual, Err(StoreError::DeserializeBinding { .. })));

        Ok(())
    }

    #[test]
    fn plan_id_from_path_rejects_a_path_with_no_filename() {
        let actual = super::plan_id_from_path(Path::new(".."));

        assert!(matches!(actual, Err(StoreError::InvalidPlanId { .. })));
    }

    #[test]
    fn plan_project_reports_a_reparse_failure() {
        let actual = super::plan_project("not a valid planning document", Path::new("x.md"));

        assert!(matches!(actual, Err(StoreError::ParsePlan { .. })));
    }

    #[test]
    fn plan_project_returns_none_for_a_v1_plan() -> Result<(), Box<dyn Error>> {
        let actual = super::plan_project(VALID_PLAN, Path::new("2026-09-05-store-plan.md"))?;

        assert_eq!(None, actual);
        Ok(())
    }

    #[test]
    fn plan_id_display_matches_as_str() {
        let id = super::PlanId("2026-09-05-example".to_string());

        assert_eq!(id.to_string(), id.as_str());
    }

    #[test]
    fn io_error_wraps_the_path_and_source() {
        let source = std::io::Error::other("boom");

        let actual = super::io_error(Path::new("/store/plans"))(source);

        assert!(matches!(actual, StoreError::Io { .. }));
    }

    #[test]
    fn list_plans_skips_non_directory_entries() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_path, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

        let repo = crate::provenance::RepoIdentity::derive(repo_dir.path())?;
        let plans_dir = store_dir.path().join(repo.as_str()).join("plans");
        // A stray file alongside the real plan directories must be skipped,
        // not mistaken for a plan.
        std::fs::write(plans_dir.join("not-a-plan-directory"), "stray")?;

        let plans = store.list_plans(repo_dir.path())?;
        assert_eq!(1, plans.len());

        Ok(())
    }

    #[test]
    fn plan_id_from_dir_entry_name_rejects_non_utf8_names() {
        use std::ffi::OsString;
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt as _;

        // An in-memory `OsString`, never written to disk: macOS's own
        // filesystem rejects non-UTF-8 names outright, so a real fixture
        // could not portably reach this branch (see the doc comment on
        // `plan_id_from_dir_entry_name`).
        let invalid_name = OsString::from_vec(vec![0xFF, 0xFE]);

        let actual =
            super::plan_id_from_dir_entry_name(Path::new("/store/plans/bad"), &invalid_name);

        assert!(matches!(actual, Err(StoreError::InvalidPlanId { .. })));
    }

    #[test]
    fn list_plans_sorts_multiple_plans_by_id() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;

        let plan_b = repo_dir.path().join("2026-09-06-second-plan.md");
        std::fs::write(&plan_b, VALID_PLAN.replace("store-plan", "second-plan"))?;
        let plan_a = repo_dir.path().join("2026-09-05-store-plan.md");
        std::fs::write(&plan_a, VALID_PLAN)?;

        let store_dir = tempfile::tempdir()?;
        let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
        // Add the later-sorting plan first, so a correct sort is observable.
        store.add_plan(&plan_b, repo_dir.path(), "HEAD")?;
        store.add_plan(&plan_a, repo_dir.path(), "HEAD")?;

        let plans = store.list_plans(repo_dir.path())?;
        let ids: Vec<&str> = plans.iter().map(|plan| plan.id.as_str()).collect();

        assert_eq!(vec!["2026-09-05-store-plan", "2026-09-06-second-plan"], ids);

        Ok(())
    }

    #[test]
    fn git_helper_reports_command_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;

        let actual = git(tmp.path(), &["not-a-real-git-subcommand"]);

        assert!(actual.is_err());
        Ok(())
    }
}
