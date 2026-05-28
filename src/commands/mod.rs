pub mod check;

/// `trove install` — interactive setup. Writes the config file, applies the
/// embedded SQL migration, and formats the JuiceFS volume. Pure decision logic
/// lives in [`install::plan`] for unit-test coverage of the state machine.
pub mod install;

/// `trove docs` — embedded walkthrough served on localhost. No native deps,
/// so it's available in the core build (not gated on `mount`).
pub mod docs;

/// `trove log/cat/diff/restore` — version-history commands (read clones via libjfs).
#[cfg(feature = "mount")]
pub mod history;

/// `trove doctor` — preflight checks (secrets, DB + pgvector + schema, backend).
#[cfg(feature = "mount")]
pub mod doctor;

/// `trove server` — single-tenant, localhost, read-only HTTP view of a store.
#[cfg(feature = "mount")]
pub mod server;

/// `trove usage` — quick DB + JuiceFS-volume size report. Mount-feature only
/// because it needs both the version DB and a JuiceFS handle.
#[cfg(feature = "mount")]
pub mod usage;

/// `trove backup` — write a local mirror of every committed file, walking
/// every revision in the version chain. Incremental by default.
#[cfg(feature = "mount")]
pub mod backup;
