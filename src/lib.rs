//! Core library for `silent-critic`.

pub mod config;
pub mod dispatch;
pub mod evaluate;
pub mod git;
pub mod ledger;
pub mod mcp_stdio;
pub mod measure;
pub mod model;
pub mod provenance;
pub mod provider;
pub mod render;
pub mod seal;
pub mod secret_file;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub mod token;
pub mod tools;
pub mod worktree;
