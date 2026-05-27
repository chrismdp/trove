//! trove — a filesystem that talks back.
//!
//! The crate is split lib + bin so the binary stays a thin CLI shell and every
//! capability is testable directly. v0.1 is the validation core; embeddings
//! search and the FUSE projection build on the same contract.

pub mod commands;
pub mod frontmatter;
pub mod types;
pub mod validate;
pub mod version;

/// JuiceFS storage binding (libjfs FFI). Behind the `mount` feature so the
/// core crate has no native dependency.
#[cfg(feature = "mount")]
pub mod jfs;

/// Copy-on-write version capture (clone into JuiceFS) + historical read.
#[cfg(feature = "mount")]
pub mod versioning;

/// The `trove embed` worker: content -> header chunks -> OpenAI -> `blob_chunks`.
#[cfg(feature = "mount")]
pub mod embed;

/// The `trove mount` FUSE filesystem, backed by `jfs`.
#[cfg(feature = "mount")]
pub mod mount;

/// `trove demo-seed`: plant a fixed, reproducible corpus for search demos.
#[cfg(feature = "mount")]
pub mod demo;
