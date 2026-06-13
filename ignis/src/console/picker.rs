//! Re-export of the neutral `interaction` picker types for console use.
//!
//! The actual type definitions live in `crate::interaction` so that non-UI
//! callers (`ask_user`, permission checker) don't depend on the console module.
pub use crate::interaction::*;
