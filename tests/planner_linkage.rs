//! Proves that `silent-critic` depends on `tftio_planner` rather than reimplementing
//! any part of its model, projection, or write path (see `REPO_INVARIANTS.md`
//! `HO-003`). This test calls a real `tftio_planner` function and asserts on
//! its typed output; a broken or removed dependency fails the build before
//! this test can even run.

use tftio_planner::parse_markdown;

const FIXTURE: &str = include_str!("fixtures/planner-linkage.md");

#[test]
fn silent_critic_links_against_tftio_planner() -> Result<(), Box<dyn std::error::Error>> {
    let plan = parse_markdown(FIXTURE)?;

    assert_eq!(plan.metadata.title, "Silent Critic planner linkage fixture");
    assert_eq!(plan.tasks.len(), 1);
    Ok(())
}
