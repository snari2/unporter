//! porthog: find and kill whatever is hogging your dev ports.
//!
//! The binary is a thin CLI over these modules; they are public so the
//! scanning and project detection can be reused or tested on their own.

pub mod project;
pub mod util;
