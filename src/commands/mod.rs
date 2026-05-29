pub mod check;

/// Internal provisioning helpers shared by `trove init`. Pure decision logic
/// lives in [`provision::plan`] for unit-test coverage of the state machine.
#[cfg(feature = "mount")]
pub mod provision;

/// `trove init` — initialise or attach the vault described by the current folder.
#[cfg(feature = "mount")]
pub mod init;

/// `trove docs` — embedded walkthrough served on localhost. No native deps,
/// so it's available in the core build (not gated on `mount`).
pub mod docs;

/// `trove self-update` — re-runs `install.sh` from the repo to upgrade in
/// place. Core build so even a `trove check`-only install can self-upgrade.
pub mod self_update;

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

/// `trove import` — take over an existing directory. Moves the original to a
/// timestamped backup, mounts trove at the original path, and streams the
/// files back through the validation gate so they get versioned + embedded.
/// The pure safety predicates (path/size guards) are core; the IO half is
/// mount-feature-gated.
pub mod import;
