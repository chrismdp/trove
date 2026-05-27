//! trove — a filesystem that talks back.
//!
//! The crate is split lib + bin so the binary stays a thin CLI shell and every
//! capability is testable directly. v0.1 is the validation core; embeddings
//! search and the FUSE projection build on the same contract.

pub mod commands;
pub mod frontmatter;
pub mod types;
pub mod validate;
