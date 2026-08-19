//! One module per kind of command: device drivers (`kettle`) and the
//! device-agnostic built-ins (`registry`).

pub mod add;
pub mod complete;
// Nothing dispatches to this yet -- the command that will is two commits
// away, and the allowance goes with it.
#[allow(dead_code)]
pub mod daemon;
pub mod kettle;
pub mod registry;
