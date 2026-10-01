//! Operator CLI entry point for `silent-critic`.
//!
//! This binary is the process edge for reading `XDG_DATA_HOME`/`HOME` (via
//! clap's `env` attribute, RS-008) and the current working directory; all
//! behavior beyond that lives in [`silent_critic::store`] and
//! [`silent_critic::provenance`].

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand, ValueEnum};
use silent_critic::dispatch::{self, DispatchConfig};
use silent_critic::measure::{self, Format as MeasureFormat};
use silent_critic::model::TaskId;
use silent_critic::render::SealDate;
use silent_critic::seal::{self, BurnedThreshold};
use silent_critic::store::{Store, StoreRoot, default_legacy_store_root, default_store_root};
use silent_critic::worktree::GitEnv;

/// Command-line arguments for the `silent-critic` operator CLI.
#[derive(Debug, Parser)]
#[command(
    name = "silent-critic",
    author,
    version,
    about = "Operator CLI for the silent-critic MCP supervision server"
)]
struct Cli {
    /// The XDG data directory; read once here per RS-008 and never inside
    /// the library.
    #[arg(long, env = "XDG_DATA_HOME", hide_env_values = true)]
    xdg_data_home: Option<PathBuf>,

    /// The user's home directory; read once here per RS-008 and never
    /// inside the library. Used only when `XDG_DATA_HOME` is unset.
    #[arg(long, env = "HOME", hide_env_values = true)]
    home: Option<PathBuf>,

    /// The operator subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// Top-level operator subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Manage plans in the out-of-repo store.
    Plan {
        /// The plan subcommand to run.
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Inspect the retired holdout store without modifying it.
    ///
    /// New state is always written under silent-critic's store root. This
    /// command deliberately exposes only read operations over the legacy
    /// store so existing holdout data remains inspectable during retirement.
    Legacy {
        /// The read-only legacy operation to run.
        #[command(subcommand)]
        command: LegacyCommand,
    },
    /// Prepare (and, in a future release, launch) a worker for a task.
    ///
    /// Currently only `--manual` is supported from this CLI: it creates the
    /// task's worktree, mints its token, renders its worker projection, and
    /// writes its MCP client config, then prints the paths and the
    /// environment an operator should export to launch a worker by hand
    /// against a harness this server cannot spawn itself. A full launch
    /// (creating all of the above *and* running a configured harness to
    /// completion) is available only through the MCP `dispatch` tool, which
    /// carries the harness configuration this CLI does not.
    Dispatch {
        /// Perform worktree creation, token minting, projection rendering,
        /// and `mcp.json` writing, but do not launch a harness: print the
        /// paths and the environment to export instead. Currently the only
        /// supported mode.
        #[arg(long)]
        manual: bool,

        /// The plan's identifier, as printed by `silent-critic plan add`.
        plan_id: String,

        /// The task's identifier, as it appears in the task graph.
        task_id: String,

        /// The repository to dispatch into (defaults to the current
        /// directory's repository).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// The git binary to invoke for worktree management.
        #[arg(long, default_value = "git")]
        git_binary: String,

        /// `PATH` for git subprocess invocations; read once here per
        /// RS-008 and never inside the library.
        #[arg(long, env = "PATH", hide_env_values = true, default_value = "")]
        git_path: String,

        /// The path to the `silent-critic-mcp` binary, written into `mcp.json`
        /// as the MCP server command a harness should run.
        #[arg(long, default_value = "silent-critic-mcp")]
        silent_critic_mcp_path: PathBuf,
    },
    /// Report the measurements that decide whether the thesis holds,
    /// derived from the stored plan(s) alone.
    ///
    /// With `PLAN_ID`, reports that one run. Without it, reports a
    /// cross-run comparison (a trajectory) across every plan stored for
    /// the current repository; `--all` widens that comparison to every
    /// plan in the store, across every repository.
    Measure {
        /// The plan's identifier, as printed by `silent-critic plan add`. Omit
        /// for a cross-run comparison.
        plan_id: Option<String>,

        /// Compare every plan in the store, across every repository,
        /// rather than only the current repository's own plans.
        #[arg(long)]
        all: bool,

        /// The rendering format.
        #[arg(long, value_enum, default_value_t = MeasureFormatArg::Human)]
        format: MeasureFormatArg,

        /// The repository whose plans to measure (defaults to the current
        /// directory's repository). Ignored with `--all`.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Seal a plan whose task graph has closed: verify every task is
    /// `done` or `abandoned`, mark the plan `implemented`, render the
    /// disclosed review artifact, and record disclosures.
    Seal {
        /// The plan's identifier, as printed by `silent-critic plan add`.
        plan_id: String,

        /// The repository the plan is stored for (defaults to the current
        /// directory's repository).
        #[arg(long)]
        repo: Option<PathBuf>,

        /// An additional path to also write the rendered artifact to (for
        /// example, a CI artifacts directory a merge-request step reads
        /// from). Validated against the same two guards as the canonical
        /// artifact path -- never inside the supervised repository's
        /// working tree, and never inside the plan's own store directory --
        /// before anything is written, exactly like the canonical path.
        #[arg(long)]
        out: Option<PathBuf>,

        /// How many times a hidden criterion's claim may be disclosed
        /// before it is reported as burned.
        #[arg(long, default_value_t = 3)]
        burned_threshold: u32,

        /// Record disclosures alone, for a plan already sealed whose
        /// disclosures were not recorded (`SealError::DisclosuresNotRecorded`,
        /// printed by an earlier `silent-critic seal` that partially failed).
        /// Does not re-verify the task graph, re-render, or rewrite the
        /// artifact; both must already exist.
        #[arg(long)]
        disclosures_only: bool,
    },
}

/// The `--format` values `silent-critic measure` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MeasureFormatArg {
    /// A compact human-readable table.
    Human,
    /// JSON, via `serde_json`.
    Json,
}

impl From<MeasureFormatArg> for MeasureFormat {
    fn from(value: MeasureFormatArg) -> Self {
        match value {
            MeasureFormatArg::Human => Self::Human,
            MeasureFormatArg::Json => Self::Json,
        }
    }
}

/// Subcommands under `silent-critic plan`.
#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Validate a plan, bind it to a repository and base ref, and store it.
    Add {
        /// Path to the plan's Markdown document.
        plan_path: PathBuf,

        /// The base ref to bind the plan to.
        #[arg(long, default_value = "HEAD")]
        base_ref: String,

        /// The repository to bind the plan to (defaults to the current
        /// directory's repository).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List the plans stored for the current repository.
    List,
    /// Print the absolute path of a stored plan.
    Path {
        /// The plan's identifier, as printed by `silent-critic plan add`.
        plan_id: String,
    },
}

/// Read-only operations over the retired holdout store.
#[derive(Debug, Subcommand)]
enum LegacyCommand {
    /// List legacy plans for the current repository.
    PlanList {
        /// The repository whose legacy plans to list (defaults to the current
        /// directory's repository).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Print the absolute path to a legacy stored plan.
    PlanPath {
        /// The legacy plan identifier.
        plan_id: String,
        /// The repository whose legacy plan to resolve (defaults to the
        /// current directory's repository).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Each store root is resolved lazily, only inside the arm that actually
    // needs it, from the same `--xdg-data-home`/`--home` inputs: a `legacy`
    // command never needs the plain store root, and no other command needs
    // the legacy one, so neither resolution's error can shadow the other's.
    let store = || -> anyhow::Result<Store> {
        let store_root = default_store_root(cli.xdg_data_home.as_deref(), cli.home.as_deref())
            .context("cannot determine a store root: set XDG_DATA_HOME or HOME")?;
        Ok(Store::new(StoreRoot::new(store_root)))
    };

    let repo_start = std::env::current_dir().context("determine current directory")?;

    let mut stdout = std::io::stdout().lock();
    match cli.command {
        Command::Plan { command } => run_plan(&store()?, &mut stdout, repo_start, command)?,
        Command::Legacy { command } => {
            let legacy_root =
                default_legacy_store_root(cli.xdg_data_home.as_deref(), cli.home.as_deref())
                    .context(
                        "cannot determine the legacy holdout store root: set XDG_DATA_HOME or HOME",
                    )?;
            let legacy_store = Store::new(StoreRoot::new(legacy_root));
            run_legacy(&legacy_store, &mut stdout, repo_start, command)?;
        }
        Command::Dispatch {
            manual,
            plan_id,
            task_id,
            repo,
            git_binary,
            git_path,
            silent_critic_mcp_path,
        } => run_dispatch(
            &store()?,
            &mut stdout,
            repo_start,
            cli.home.unwrap_or_default().as_path(),
            manual,
            &plan_id,
            &task_id,
            repo,
            git_binary,
            git_path,
            silent_critic_mcp_path,
        )?,
        Command::Measure {
            plan_id,
            all,
            format,
            repo,
        } => run_measure(
            &store()?,
            &mut stdout,
            repo_start,
            plan_id,
            all,
            MeasureFormat::from(format),
            repo,
        )?,
        Command::Seal {
            plan_id,
            repo,
            out,
            burned_threshold,
            disclosures_only,
        } => {
            let repo_start = repo.unwrap_or(repo_start);
            run_seal(
                &store()?,
                &repo_start,
                &plan_id,
                out.as_deref(),
                burned_threshold,
                disclosures_only,
                &mut stdout,
            )?;
        }
    }

    Ok(())
}

/// Run a `silent-critic plan` operation. Split out of `main` (alongside
/// `run_legacy`) to keep it under this crate's line-count lint now that
/// `main` also resolves the plain store root lazily rather than
/// unconditionally up front.
fn run_plan(
    store: &Store,
    stdout: &mut impl Write,
    repo_start: PathBuf,
    command: PlanCommand,
) -> anyhow::Result<()> {
    match command {
        PlanCommand::Add {
            plan_path,
            base_ref,
            repo,
        } => {
            let repo_start = repo.unwrap_or(repo_start);
            let plan_id = store
                .add_plan(&plan_path, &repo_start, &base_ref)
                .with_context(|| format!("adding plan {}", plan_path.display()))?;
            writeln!(stdout, "{plan_id}").context("write plan id to stdout")?;
        }
        PlanCommand::List => {
            let plans = store
                .list_plans(&repo_start)
                .context("listing plans for the current repository")?;
            for plan in plans {
                match &plan.provenance.project {
                    Some(slug) => writeln!(stdout, "{} project={slug}", plan.id)
                        .context("write plan id and project to stdout")?,
                    None => {
                        writeln!(stdout, "{}", plan.id).context("write plan id to stdout")?;
                    }
                }
            }
        }
        PlanCommand::Path { plan_id } => {
            let path = store
                .plan_path(&repo_start, &plan_id)
                .with_context(|| format!("resolving plan path for {plan_id}"))?;
            writeln!(stdout, "{}", path.display()).context("write plan path to stdout")?;
        }
    }
    Ok(())
}

/// Run a read-only legacy store operation.
fn run_legacy(
    legacy_store: &Store,
    stdout: &mut impl Write,
    repo_start: PathBuf,
    command: LegacyCommand,
) -> anyhow::Result<()> {
    match command {
        LegacyCommand::PlanList { repo } => {
            let repo_start = repo.unwrap_or(repo_start);
            let plans = legacy_store
                .list_plans(&repo_start)
                .context("listing legacy plans for the current repository")?;
            for plan in plans {
                writeln!(stdout, "{}", plan.id).context("write legacy plan id to stdout")?;
            }
        }
        LegacyCommand::PlanPath { plan_id, repo } => {
            let repo_start = repo.unwrap_or(repo_start);
            let path = legacy_store
                .plan_path(&repo_start, &plan_id)
                .with_context(|| format!("resolving legacy plan path for {plan_id}"))?;
            writeln!(stdout, "{}", path.display()).context("write legacy plan path to stdout")?;
        }
    }
    Ok(())
}

/// Handle `silent-critic dispatch --manual`: create the task's worktree, mint its
/// token, render its worker projection, write its MCP client config, and
/// print the paths and environment an operator should export. Split out of
/// `main` to keep it under this crate's line-count lint (a pre-existing
/// arm's body moved verbatim, with no behavior change, to make room for
/// `Command::Measure` alongside it).
#[allow(clippy::too_many_arguments)]
fn run_dispatch(
    store: &Store,
    stdout: &mut impl std::io::Write,
    repo_start: PathBuf,
    home: &Path,
    manual: bool,
    plan_id: &str,
    task_id: &str,
    repo: Option<PathBuf>,
    git_binary: String,
    git_path: String,
    silent_critic_mcp_path: PathBuf,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        manual,
        "only `--manual` dispatch is supported from this CLI; a full launch \
         (with a configured harness) happens through the MCP `dispatch` tool"
    );
    let repo_start = repo.unwrap_or(repo_start);
    let config = DispatchConfig {
        git: GitEnv::new(git_binary, git_path, home.to_string_lossy().into_owned()),
        silent_critic_mcp_path,
    };
    let plan_path = store
        .plan_path(&repo_start, plan_id)
        .with_context(|| format!("resolving plan path for {plan_id}"))?;
    let plan_source = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("reading plan {}", plan_path.display()))?;
    let parsed_plan = tftio_planner::parse_markdown(&plan_source)
        .with_context(|| format!("parsing plan {}", plan_path.display()))?;
    anyhow::ensure!(
        parsed_plan
            .tasks
            .iter()
            .any(|task| task.id.as_str() == task_id),
        "no such task {task_id} in plan {plan_id}"
    );
    let task = TaskId::new(task_id.to_owned());

    let prepared = dispatch::prepare(store, &repo_start, plan_id, &task, &plan_source, &config)
        .with_context(|| format!("preparing dispatch for task {task_id}"))?;

    writeln!(stdout, "worktree: {}", prepared.worktree_path.display())
        .context("write worktree path to stdout")?;
    writeln!(stdout, "brief: {}", prepared.brief_path.display())
        .context("write brief path to stdout")?;
    writeln!(stdout, "mcp config: {}", prepared.mcp_path.display())
        .context("write mcp config path to stdout")?;
    writeln!(
        stdout,
        "export the following before launching a worker by hand \
         (the harness process itself receives only SILENT_CRITIC_BRIEF, \
         SILENT_CRITIC_REPO, and SILENT_CRITIC_MCP_CONFIG; the rest live only \
         in mcp.json for silent-critic-mcp):"
    )
    .context("write export header to stdout")?;
    for (name, value) in &prepared.mcp_env {
        writeln!(stdout, "  export {name}={value}")
            .context("write exported environment to stdout")?;
    }
    writeln!(
        stdout,
        "  export SILENT_CRITIC_MCP_CONFIG={}",
        prepared.mcp_path.display()
    )
    .context("write exported environment to stdout")?;
    Ok(())
}

/// Handle `silent-critic measure`: one run (`plan_id`), a cross-run comparison for
/// the current repository (neither `plan_id` nor `all`), or a comparison
/// across every repository in the store (`all`). Split out of `main` to
/// keep it under this crate's line-count lint.
#[allow(clippy::too_many_arguments)]
fn run_measure(
    store: &Store,
    stdout: &mut impl std::io::Write,
    repo_start: PathBuf,
    plan_id: Option<String>,
    all: bool,
    format: MeasureFormat,
    repo: Option<PathBuf>,
) -> anyhow::Result<()> {
    if all {
        let trajectories =
            measure::trajectory_for_store(store).context("comparing every plan in the store")?;
        writeln!(
            stdout,
            "{}",
            measure::render_store_trajectories(&trajectories, format)
        )
        .context("write grouped trajectories to stdout")?;
    } else if let Some(plan_id) = plan_id {
        let repo_start = repo.unwrap_or(repo_start);
        let measurement = measure::measure_stored(store, &repo_start, &plan_id)
            .with_context(|| format!("measuring plan {plan_id}"))?;
        writeln!(stdout, "{}", measure::render(&measurement, format))
            .context("write measurement to stdout")?;
    } else {
        let repo_start = repo.unwrap_or(repo_start);
        let trajectory = measure::trajectory_for_repository(store, &repo_start)
            .context("comparing the current repository's stored plans")?;
        writeln!(
            stdout,
            "{}",
            measure::render_trajectory(&trajectory, format)
        )
        .context("write trajectory to stdout")?;
    }
    Ok(())
}

/// The `silent-critic seal` subcommand's body, split out of `main` so `main`
/// itself stays under clippy's line-count lint for a function
/// (`too_many_lines`) rather than growing another `match` arm's worth of
/// inline logic.
#[allow(clippy::too_many_arguments)]
fn run_seal(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    out: Option<&Path>,
    burned_threshold: u32,
    disclosures_only: bool,
    stdout: &mut impl Write,
) -> anyhow::Result<()> {
    let date = SealDate::new(today_utc()).context("build today's date for the sealed artifact")?;
    let threshold = BurnedThreshold::new(burned_threshold);

    if disclosures_only {
        let outcome = seal::seal_disclosures_only(store, repo_start, plan_id, &date, threshold)
            .with_context(|| format!("recording disclosures for already-sealed plan {plan_id}"))?;
        match outcome {
            seal::DisclosuresOutcome::Recorded { burned } => {
                writeln!(stdout, "disclosures recorded for {plan_id}")
                    .context("write disclosures-only confirmation to stdout")?;
                print_burned(&burned);
            }
            seal::DisclosuresOutcome::AlreadyRecorded => {
                writeln!(
                    stdout,
                    "disclosures already recorded for {plan_id}; nothing to do"
                )
                .context("write disclosures-only confirmation to stdout")?;
            }
        }
        return Ok(());
    }

    // `seal` itself validates and writes `out` (both `reject_if_inside`
    // guards, the same as the canonical artifact path) as part of its own
    // load-bearing write-before-mutate order -- this CLI never writes
    // either path itself.
    let outcome = seal::seal(store, repo_start, plan_id, &date, threshold, out)
        .with_context(|| format!("sealing plan {plan_id}"))?;

    writeln!(stdout, "artifact: {}", outcome.artifact_path.display())
        .context("write artifact path to stdout")?;
    if let Some(out_path) = &outcome.out_path {
        writeln!(stdout, "artifact copy: {}", out_path.display())
            .context("write artifact copy path to stdout")?;
    }
    print_burned(&outcome.burned);
    Ok(())
}

/// Print one `burned: <claim> (disclosed N times)` line per burned
/// criterion to stderr -- shared by `run_seal`'s normal and
/// `--disclosures-only` paths.
fn print_burned(burned: &[silent_critic::seal::BurnedCriterion]) {
    for criterion in burned {
        eprintln!(
            "burned: {} (disclosed {} times)",
            criterion.claim, criterion.count
        );
    }
}

/// Today's UTC date in `YYYY-MM-DD` form, read from the clock once here at
/// the process edge (the CLI is the process boundary [`silent_critic::render`]'s
/// module docs describe: rendering itself takes a typed [`SealDate`], never
/// a clock read from inside it).
///
/// Duplicated from `src/ledger.rs`'s identical `today_utc`/
/// `civil_from_days` pair (and `src/tools.rs`'s own copy) rather than
/// shared: all three are small, self-contained, and this one is the only
/// copy that lives at a process edge rather than inside the library.
fn today_utc() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`, duplicated from `src/ledger.rs` for
/// the same reason as [`today_utc`].
const fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_from_days_matches_known_dates_across_both_month_halves() {
        // Mirrors `src/ledger.rs`'s identical test for its own copy of this
        // conversion: `days_since_epoch = 0` (1970-01-01) exercises the
        // `month <= 2` half (`month_index >= 10`); `2000-03-01` exercises
        // the other half. `today_utc` itself is driven by the real clock,
        // so it alone would exercise only whichever half today happens to
        // fall in.
        assert_eq!((1970, 1, 1), civil_from_days(0));
        assert_eq!((2000, 3, 1), civil_from_days(11_017));
    }
}
