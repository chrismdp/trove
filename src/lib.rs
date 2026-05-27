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

/// Content-addressed version-blob store over Cloudflare R2.
#[cfg(feature = "mount")]
pub mod blobstore;

/// Best-effort, eventually-consistent version recorder (WAL + drainer).
#[cfg(feature = "mount")]
pub mod recorder;

/// The `trove mount` FUSE filesystem, backed by `jfs`.
#[cfg(feature = "mount")]
pub mod mount;
