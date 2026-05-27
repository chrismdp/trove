pub mod check;

/// `trove log/cat/diff/restore` — version-history commands (read clones via libjfs).
#[cfg(feature = "mount")]
pub mod history;

/// `trove doctor` — preflight checks (secrets, DB + pgvector + schema, backend).
#[cfg(feature = "mount")]
pub mod doctor;

/// `trove server` — single-tenant, localhost, read-only HTTP view of a store.
#[cfg(feature = "mount")]
pub mod server;
