//! `trove init` — folder-aware setup. Derives the vault's names from the folder,
//! resolves + **validates** credentials (prompting with guidance at a TTY, or
//! reading the environment and printing a setup guide otherwise), probes the
//! backend, then creates a new vault or attaches to an existing one and mounts
//! at the cwd.
//!
//! Credentials are validated *as they're entered* and only persisted once they
//! work (proposal: "validate as you go, plain errors"):
//!
//! - the DB URL is **connected** (a scheme check first, then a real connect),
//! - the R2 endpoint + keys are **exercised** with a signed `ListObjectsV2`.
//!
//! A value that fails is re-prompted in the same session (readline editing), so
//! a typo never wedges you into a re-run loop. And when the creds are valid but
//! the bucket simply doesn't exist yet, they're **saved before** we bail — so
//! after you create the bucket the re-run doesn't ask again. The classic
//! Supabase trap (the IPv6-only `db.<ref>.supabase.co` *direct* host) is called
//! out at the prompt and on failure.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use postgres::{Client, NoTls};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::commands::provision::{self, FormatAction, MigrationAction, ProvisionFlags};
use crate::config::{self, CredProfile, Credentials, VolumeConfig};
use crate::s3::{BucketProbe, BucketState};

pub struct InitOptions {
    pub no_embed: bool,
    /// Credential profile to attach this volume under. `None` ⇒ the default
    /// (top-level) credentials — the common, single-account case. A named
    /// profile is prompted for and saved under `[profiles.<name>]` if new.
    pub profile: Option<String>,
}

pub struct InitMount {
    pub volume: String,
    pub schema: String,
    pub bucket: String,
    pub mountpoint: PathBuf,
    pub meta: String,
    pub cache: String,
    pub versions_db: String,
    /// True when the R2 keys came from the environment and were *not* written to
    /// `credentials.toml` (the op/1Password flow). A boot agent runs with a bare
    /// environment, so auto-mount can't reach the object store — the caller
    /// warns before installing one.
    pub keys_in_env_only: bool,
}

/// R2 inputs that passed a live probe, plus where the keys came from.
struct R2Resolved {
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    access_from_env: bool,
    secret_from_env: bool,
    bucket_state: BucketState,
}

pub fn run(opts: InitOptions) -> Result<InitMount> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    ensure_empty_for_init(&cwd)?;
    let basename = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("current directory has no usable basename"))?;
    let volume = config::normalise_volume_name(basename)?;
    let schema = config::schema_for(&volume);
    let bucket_name = config::bucket_for(&volume)?;

    let tty = io::stdin().is_terminal();
    let mut creds = Credentials::load();

    // The credential profile this volume attaches under. `default`/empty ⇒ the
    // top-level (default) creds; a named profile is independent (its own
    // account) and never consults env — the keys must live in the file.
    let profile = opts
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "default");
    let consult_env = profile.is_none();

    println!(
        "{} folder `{}` -> schema `{}` · bucket `{}`{}",
        "trove init:".bold(),
        basename,
        schema.cyan(),
        bucket_name.cyan(),
        profile
            .map(|p| format!(" · profile {}", p.cyan()))
            .unwrap_or_default()
    );

    // Seed the resolver from the file: the named profile if one is requested,
    // else the default (top-level) creds. Env is folded in by the resolvers
    // themselves, but only for the default profile.
    let seed = match profile {
        Some(name) => creds.profiles.get(name).cloned().unwrap_or_default(),
        None => CredProfile {
            versions_db: creds.versions_db.clone(),
            r2_endpoint: creds.r2_endpoint.clone(),
            r2_access_key_id: creds.r2_access_key_id.clone(),
            r2_secret_access_key: creds.r2_secret_access_key.clone(),
        },
    };

    // With no TTY we can't prompt, so everything must already resolve. For the
    // default profile that means env/creds (print the agent guide); for a named
    // profile it means a complete `[profiles.<name>]` block.
    if !tty {
        let seen_db = seed.versions_db.is_some()
            || (consult_env
                && (provision::env_nonempty("TROVE_VERSIONS_DB").is_some()
                    || provision::env_nonempty("DATABASE_URL").is_some()));
        let seen_ep = seed.r2_endpoint.is_some()
            || (consult_env && provision::env_nonempty("TROVE_R2_ENDPOINT").is_some());
        let seen_ak = seed.r2_access_key_id.is_some()
            || (consult_env && provision::env_nonempty("R2_ACCESS_KEY_ID").is_some());
        let seen_sk = seed.r2_secret_access_key.is_some()
            || (consult_env && provision::env_nonempty("R2_SECRET_ACCESS_KEY").is_some());
        if !(seen_db && seen_ep && seen_ak && seen_sk) {
            match profile {
                None => {
                    print_agent_guide(seen_db, seen_ep, seen_ak, seen_sk);
                    bail!(
                        "not enough configuration in the environment and no TTY to prompt — \
                         set the variables above, then re-run `trove init`"
                    );
                }
                Some(name) => bail!(
                    "credential profile `{name}` is incomplete and there's no TTY to prompt — \
                     add a full `[profiles.{name}]` block (versions_db + r2 endpoint/keys) to \
                     credentials.toml, then re-run."
                ),
            }
        }
    }

    // Resolve + validate each credential, re-prompting on failure (at a TTY).
    let (versions_db, mut client) = resolve_and_connect_db(seed.versions_db.clone(), consult_env, tty)?;
    let r2 = resolve_and_probe_r2(
        seed.r2_endpoint.clone(),
        seed.r2_access_key_id.clone(),
        seed.r2_secret_access_key.clone(),
        consult_env,
        tty,
        &bucket_name,
    )?;

    // Everything that *can* be checked has passed → persist the creds. (Done
    // even when the bucket is missing: the creds are valid, so the re-run after
    // you create the bucket won't ask again.) The keys must also reach libjfs
    // this run, so push them into the environment regardless of source.
    std::env::set_var("R2_ACCESS_KEY_ID", &r2.access_key);
    std::env::set_var("R2_SECRET_ACCESS_KEY", &r2.secret_key);
    match profile {
        None => {
            // Default profile: keep env the source of truth for the keys — only
            // persist a key we gathered ourselves (typed at the prompt).
            creds.versions_db = Some(versions_db.clone());
            creds.r2_endpoint = Some(r2.endpoint.clone());
            if !r2.access_from_env {
                creds.r2_access_key_id = Some(r2.access_key.clone());
            }
            if !r2.secret_from_env {
                creds.r2_secret_access_key = Some(r2.secret_key.clone());
            }
        }
        Some(name) => {
            // Named profile: self-contained (no env source), so persist all four
            // so the boot agent can resolve them with nothing in the shell.
            creds.profiles.insert(
                name.to_string(),
                CredProfile {
                    versions_db: Some(versions_db.clone()),
                    r2_endpoint: Some(r2.endpoint.clone()),
                    r2_access_key_id: Some(r2.access_key.clone()),
                    r2_secret_access_key: Some(r2.secret_key.clone()),
                },
            );
        }
    }
    let cred_path = creds.save()?;
    println!("{} saved credentials to {}", "trove init:".bold(), cred_path.display());

    if matches!(r2.bucket_state, BucketState::Missing) {
        bail!(
            "bucket `{bucket_name}` doesn't exist yet — create it once in your R2 \
             dashboard (trove uses the whole bucket), then re-run `trove init`. \
             Your credentials are saved, so it won't ask again."
        );
    }

    let bucket = r2.bucket;
    client
        .batch_execute("create extension if not exists vector")
        .context("creating the pgvector extension")?;
    let schema_exists: bool = client
        .query_one("select to_regnamespace($1) is not null", &[&schema])?
        .get(0);

    match (schema_exists, r2.bucket_state) {
        (true, BucketState::NonEmpty) => {
            select_schema(&mut client, &schema)?;
            let db_state = provision::inspect_db(&mut client, &schema)?;
            for table in ["blobs", "file_versions", "blob_chunks"] {
                if !db_state.tables_present.contains(table) {
                    bail!("schema `{schema}` exists but is not a Trove vault — missing `{table}`");
                }
            }
            if !db_state.jfs_present {
                bail!("schema `{schema}` exists but has no JuiceFS metadata tables");
            }
            match db_state.recorded_bucket.as_deref() {
                Some(recorded) if recorded == bucket => {}
                Some(recorded) => bail!(
                    "schema `{schema}` is formatted on bucket `{recorded}`, not derived bucket `{bucket}`"
                ),
                None => bail!(
                    "schema `{schema}` has JuiceFS metadata but no recorded bucket; refusing to attach"
                ),
            }
            confirm_attach(&volume)?;
            println!(
                "{} found existing vault `{volume}` — attaching",
                "trove init:".bold()
            );
        }
        (false, BucketState::Empty) => {
            create_schema(&mut client, &schema)?;
            provision::apply_migration(
                &mut client,
                &MigrationAction::RunMigration,
                ProvisionFlags::default(),
            )?;
            let meta = config::juicefs_meta_url(&versions_db, &schema);
            provision::apply_format(
                &mut client,
                &FormatAction::Format,
                &volume,
                &meta,
                &bucket,
                &schema,
                ProvisionFlags::default(),
            )?;
        }
        (true, BucketState::Empty) => {
            bail!("schema `{schema}` exists but bucket `{bucket_name}` is empty — clear the stray schema or use another folder name")
        }
        (false, BucketState::NonEmpty) => {
            bail!("bucket `{bucket_name}` is non-empty but schema `{schema}` does not exist — clear the stray bucket or use another folder name")
        }
        (_, BucketState::Missing) => unreachable!("handled above"),
    }

    let cache =
        provision::env_nonempty("TROVE_CACHE").unwrap_or_else(|| "/tmp/trove-cache".to_string());
    let mountpoint = cwd.canonicalize().unwrap_or(cwd);
    let vol_cfg = VolumeConfig {
        bucket: bucket.clone(),
        schema: schema.clone(),
        mountpoint: mountpoint.to_string_lossy().into_owned(),
        cache: cache.clone(),
        credentials: profile.map(|s| s.to_string()),
    };
    let vol_path = vol_cfg.save(&volume)?;
    println!("{} wrote {}", "trove init:".bold(), vol_path.display());

    if opts.no_embed {
        println!("{} embedding disabled for this mount", "trove init:".bold());
    }

    // Only the default profile defers keys to env; named profiles always
    // persist all four, so their boot agent is self-sufficient.
    let keys_in_env_only = profile.is_none() && (r2.access_from_env || r2.secret_from_env);

    Ok(InitMount {
        volume,
        schema,
        bucket,
        mountpoint,
        meta: versions_db.clone(),
        cache,
        versions_db,
        keys_in_env_only,
    })
}

// -- Credential resolution + validation -----------------------------------

/// DB URL precedence: env (`TROVE_VERSIONS_DB`, then `DATABASE_URL`) → creds
/// file → interactive prompt. The candidate is **connected** before being
/// accepted; a failure re-prompts (at a TTY) so a typo is fixed in place.
fn resolve_and_connect_db(
    file_seed: Option<String>,
    consult_env: bool,
    tty: bool,
) -> Result<(String, Client)> {
    let mut candidate = if consult_env {
        provision::env_nonempty("TROVE_VERSIONS_DB")
            .or_else(|| provision::env_nonempty("DATABASE_URL"))
            .or(file_seed)
    } else {
        file_seed
    };
    let mut explained = false;
    loop {
        let url = match candidate.take() {
            Some(v) => v,
            None => {
                if !explained {
                    explain(
                        "Postgres database — metadata, version history and embeddings.",
                        &[
                            "Easiest is Supabase (free tier): create a project, then Connect →",
                            "Connection string → Session pooler (host ends .pooler.supabase.com,",
                            "port 5432). Paste that URI — it includes your DB password.",
                            "Avoid the 'Direct connection' db.<ref>.supabase.co host: it is",
                            "IPv6-only and usually unreachable.",
                        ],
                    );
                    explained = true;
                }
                ask_required("versions_db (postgres URL)")?
            }
        };
        match connect_db(&url) {
            Ok(client) => {
                println!("  {} database reachable", "✓".green());
                return Ok((url, client));
            }
            Err(e) => {
                eprintln!("  {} {e}", "✗".red());
                if !tty {
                    return Err(e);
                }
                // candidate is already None → loop re-prompts.
            }
        }
    }
}

/// Resolve the R2 endpoint + keys and **exercise** them with a signed
/// `ListObjectsV2`. A bad endpoint shape or a failed probe (403 / unreachable)
/// re-prompts the trio at a TTY. A `404` (bucket not created yet) is *success*
/// for the credentials — it's returned as [`BucketState::Missing`].
fn resolve_and_probe_r2(
    file_ep: Option<String>,
    file_ak: Option<String>,
    file_sk: Option<String>,
    consult_env: bool,
    tty: bool,
    bucket_name: &str,
) -> Result<R2Resolved> {
    let ak_env = consult_env.then(|| provision::env_nonempty("R2_ACCESS_KEY_ID")).flatten();
    let sk_env = consult_env
        .then(|| provision::env_nonempty("R2_SECRET_ACCESS_KEY"))
        .flatten();
    let mut ep_c = if consult_env {
        provision::env_nonempty("TROVE_R2_ENDPOINT")
            .or_else(|| provision::env_nonempty("TROVE_R2_BUCKET"))
            .or(file_ep)
    } else {
        file_ep
    };
    let mut ak_c = ak_env.clone().or(file_ak);
    let mut sk_c = sk_env.clone().or(file_sk);
    let mut explained_ep = false;
    let mut explained_key = false;

    loop {
        let endpoint_input = match ep_c.take() {
            Some(v) => v,
            None => {
                if !explained_ep {
                    explain(
                        "R2 account endpoint — the account's S3 endpoint (NOT a bucket URL).",
                        &[
                            "Cloudflare dashboard → R2 → API → S3 endpoint:",
                            "  https://<accountid>.r2.cloudflarestorage.com",
                            "trove appends the bucket name itself.",
                        ],
                    );
                    explained_ep = true;
                }
                ask_required("R2 endpoint")?
            }
        };
        let (endpoint, bucket) =
            match config::r2_endpoint_from_bucket_input(&endpoint_input, bucket_name) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("  {} {e}", "✗".red());
                    if !tty {
                        return Err(e);
                    }
                    continue; // re-prompt the endpoint only
                }
            };

        let access_key = match ak_c.take() {
            Some(v) => v,
            None => {
                if !explained_key {
                    explain(
                        "R2 API token — Object Read & Write (no admin scope needed).",
                        &[
                            "Cloudflare → R2 → Manage R2 API Tokens → Create. It shows an",
                            "Access Key ID + Secret Access Key (the secret is shown once).",
                        ],
                    );
                    explained_key = true;
                }
                prompt_secret_required("R2_ACCESS_KEY_ID")?
            }
        };
        let secret_key = match sk_c.take() {
            Some(v) => v,
            None => prompt_secret_required("R2_SECRET_ACCESS_KEY")?,
        };

        let probe = BucketProbe {
            endpoint: bucket.clone(),
            access_key: access_key.clone(),
            secret_key: secret_key.clone(),
        }
        .probe();
        match probe {
            Ok(state) => {
                let note = match state {
                    BucketState::Missing => "bucket not created yet".yellow(),
                    BucketState::Empty => "bucket empty — will create a new vault".dimmed(),
                    BucketState::NonEmpty => "bucket has data — existing vault".dimmed(),
                };
                println!("  {} object store reachable ({note})", "✓".green());
                return Ok(R2Resolved {
                    endpoint,
                    bucket,
                    access_key,
                    secret_key,
                    access_from_env: ak_env.is_some(),
                    secret_from_env: sk_env.is_some(),
                    bucket_state: state,
                });
            }
            Err(e) => {
                eprintln!("  {} object store: {e}", "✗".red());
                if !tty {
                    return Err(e);
                }
                // Could be the endpoint or either key — re-ask all three.
                println!("  re-enter the R2 endpoint and keys:");
                ep_c = None;
                ak_c = None;
                sk_c = None;
            }
        }
    }
}

/// Connect to the version DB. Validates the URL's scheme first, then connects,
/// turning the most common failure (Supabase's IPv6-only *direct* host) into
/// actionable guidance rather than a bare DNS error.
fn connect_db(url: &str) -> Result<Client> {
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        bail!(
            "the database URL must start with `postgres://` (or `postgresql://`) — \
             paste the Supabase Session-pooler connection string"
        );
    }
    Client::connect(url, NoTls).map_err(|e| {
        let mut msg = format!("connecting to the database failed: {e}");
        if is_supabase_direct(url) {
            msg.push_str(
                "\n     this looks like Supabase's *direct* connection (db.<ref>.supabase.co), \
                 which is IPv6-only and usually unreachable.\n     \
                 Use the Session pooler instead: Supabase → Connect → Connection string → \
                 Session pooler (host ends .pooler.supabase.com, port 5432).",
            );
        }
        anyhow::anyhow!(msg)
    })
}

/// Heuristic for "this is Supabase's direct (non-pooler) host".
fn is_supabase_direct(url: &str) -> bool {
    url.contains("@db.") && url.contains(".supabase.co")
}

/// The non-interactive setup guide — printed when `trove init` runs with no TTY
/// and an incomplete environment, with a ✓/✗ for each required variable.
fn print_agent_guide(db: bool, ep: bool, ak: bool, sk: bool) {
    let mark = |present: bool| if present { "✓".green() } else { "✗".red() };
    println!("{}", "trove init — non-interactive (no TTY detected)".bold());
    println!(
        "Reading credentials from the environment / credentials.toml. Set the\n\
         variables below, then re-run `trove init` in your vault folder.\n"
    );
    println!("{}", "Required (shared across all your vaults)".bold());
    println!(
        "  {} TROVE_VERSIONS_DB     Postgres URL — metadata, history, embeddings (also DATABASE_URL).",
        mark(db)
    );
    println!("                          Supabase: use the Session pooler (…pooler.supabase.com:5432),");
    println!("                          NOT the IPv6-only db.<ref>.supabase.co direct host.");
    println!("  {} TROVE_R2_ENDPOINT     R2 account endpoint, e.g.", mark(ep));
    println!("                          https://<accountid>.r2.cloudflarestorage.com");
    println!(
        "  {} R2_ACCESS_KEY_ID      R2 API token access key id (Object Read & Write).",
        mark(ak)
    );
    println!(
        "  {} R2_SECRET_ACCESS_KEY  R2 API token secret (shown once at creation).",
        mark(sk)
    );
    println!("\n{}", "Optional".bold());
    println!("  OPENAI_API_KEY        Embeddings + `trove search`. Omit → mounts with embedding off.");
    println!("  TROVE_CACHE           Local block-cache dir (default: /tmp/trove-cache).");
    println!("\nThe bucket `trove-<folder>` must already exist in your R2 dashboard.");
}

// -- Prompt helpers -------------------------------------------------------

/// Print a short guidance block before a prompt.
fn explain(header: &str, body: &[&str]) {
    println!("\n{}", header.bold());
    for line in body {
        println!("  {line}");
    }
}

/// Prompt (with readline editing) until a non-empty value is entered.
fn ask_required(label: &str) -> Result<String> {
    loop {
        let line = read_input_line(&format!("{label}: "))?;
        let v = line.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
        println!("  {} required — please enter a value.", "·".dimmed());
    }
}

/// Read one line with readline-style editing (arrow keys, ^U/^K) — what people
/// expect when fixing a pasted connection string.
fn read_input_line(prompt: &str) -> Result<String> {
    use rustyline::error::ReadlineError;
    let mut editor = rustyline::DefaultEditor::new()
        .map_err(|e| anyhow::anyhow!("initialising the line editor: {e}"))?;
    match editor.readline(prompt) {
        Ok(line) => Ok(line),
        Err(ReadlineError::Eof) => Ok(String::new()),
        Err(ReadlineError::Interrupted) => bail!("aborted (Ctrl-C)"),
        Err(e) => Err(anyhow::anyhow!("reading input: {e}")),
    }
}

/// Prompt once (hidden) for a required secret; confirm receipt with a char count
/// since the prompt echoes nothing.
fn prompt_secret_required(name: &str) -> Result<String> {
    loop {
        let v = read_secret_line(&format!(
            "{name} (hidden — you won't see it as you type): "
        ))?;
        let v = v.trim().to_string();
        if !v.is_empty() {
            println!("  {} {name} set ({} chars)", "✓".green(), v.chars().count());
            return Ok(v);
        }
        println!("  {} required — please paste the value.", "·".dimmed());
    }
}

/// Read a line with terminal echo disabled, restoring the prior state after.
fn read_secret_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        let have_term = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
        let saved = term;
        if have_term {
            term.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
        }
        let mut line = String::new();
        let res = io::stdin().read_line(&mut line);
        if have_term {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
            println!();
        }
        res?;
        Ok(line)
    }
    #[cfg(not(unix))]
    {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(line)
    }
}

fn create_schema(client: &mut Client, schema: &str) -> Result<()> {
    let ident = schema.replace('"', "\"\"");
    client
        .batch_execute(&format!(
            "create schema if not exists \"{ident}\"; set search_path to \"{ident}\", public, extensions;"
        ))
        .with_context(|| format!("creating/selecting schema {schema}"))
}

fn select_schema(client: &mut Client, schema: &str) -> Result<()> {
    let ident = schema.replace('"', "\"\"");
    client
        .batch_execute(&format!(
            "set search_path to \"{ident}\", public, extensions;"
        ))
        .with_context(|| format!("selecting schema {schema}"))
}

fn confirm_attach(volume: &str) -> Result<()> {
    if !io::stdin().is_terminal() {
        return Ok(());
    }
    print!("Found vault `{volume}` — attach to it? [Y/n] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    if answer.is_empty() || answer == "y" || answer == "yes" {
        Ok(())
    } else {
        bail!("aborted — not attaching to existing vault `{volume}`")
    }
}

fn ensure_empty_for_init(path: &Path) -> Result<()> {
    let visible = std::fs::read_dir(path)
        .with_context(|| format!("opening {}", path.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().chars().next() != Some('.'))
        .count();
    if visible > 0 {
        bail!(
            "{} is not empty — run `trove import {}` to adopt existing files",
            path.display(),
            path.display()
        );
    }
    Ok(())
}
