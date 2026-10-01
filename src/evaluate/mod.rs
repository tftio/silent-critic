//! Criterion evaluators.
//!
//! Each way a criterion can be evaluated gets its own submodule. [`automated`]
//! is the path most criteria should take: it runs a criterion's shell check
//! directly, with no model in the loop, and records the result as
//! tool-authored evidence.

pub mod automated;
pub mod judge;
