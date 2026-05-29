use anyhow::{bail, Context, Result};
use colored::Colorize;
use postgres::{Client, NoTls};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::commands::provision::{self, FormatAction, MigrationAction, ProvisionFlags};
use crate::config::{self, Credentials, VolumeConfig};
use crate::s3::{BucketProbe, BucketState};

pub struct InitOptions {
    pub no_embed: bool,
}

pub struct InitMount {
    pub volume: String,
    pub schema: String,
    pub bucket: String,
    pub mountpoint: PathBuf,
    pub meta: String,
    pub cache: String,
    pub versions_db: String,
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

    let mut creds = Credentials::load();
    let versions_db = provision::env_nonempty("TROVE_VERSIONS_DB")
        .or_else(|| provision::env_nonempty("DATABASE_URL"))
        .or_else(|| creds.versions_db.clone());
    let versions_db = resolve_or_prompt(
        versions_db,
        "versions_db (postgres URL)",
        None,
        "no DB URL — set TROVE_VERSIONS_DB or run `trove init` in a terminal",
    )?;
    let r2_input = provision::env_nonempty("TROVE_R2_BUCKET")
        .or_else(|| provision::env_nonempty("TROVE_R2_ENDPOINT"))
        .or_else(|| creds.r2_endpoint.clone());
    let r2_input = resolve_or_prompt(
        r2_input,
        "R2 endpoint",
        Some("https://<account>.r2.cloudflarestorage.com"),
        "no R2 endpoint — set TROVE_R2_ENDPOINT or TROVE_R2_BUCKET, or run `trove init` in a terminal",
    )?;
    let (r2_endpoint, bucket) = config::r2_endpoint_from_bucket_input(&r2_input, &bucket_name)?;
    let access_key_env = provision::env_nonempty("R2_ACCESS_KEY_ID");
    let access_key = access_key_env
        .clone()
        .or_else(|| creds.r2_access_key_id.clone());
    let access_key = resolve_secret_or_prompt(
        access_key,
        "R2_ACCESS_KEY_ID",
        "no R2 access key — set R2_ACCESS_KEY_ID or run `trove init` in a terminal",
    )?;
    let secret_key_env = provision::env_nonempty("R2_SECRET_ACCESS_KEY");
    let secret_key = secret_key_env
        .clone()
        .or_else(|| creds.r2_secret_access_key.clone());
    let secret_key = resolve_secret_or_prompt(
        secret_key,
        "R2_SECRET_ACCESS_KEY",
        "no R2 secret key — set R2_SECRET_ACCESS_KEY or run `trove init` in a terminal",
    )?;
    creds.versions_db = Some(versions_db.clone());
    creds.r2_endpoint = Some(r2_endpoint.clone());
    if access_key_env.is_none() {
        creds.r2_access_key_id = Some(access_key.clone());
    }
    if secret_key_env.is_none() {
        creds.r2_secret_access_key = Some(secret_key.clone());
    }
    std::env::set_var("R2_ACCESS_KEY_ID", &access_key);
    std::env::set_var("R2_SECRET_ACCESS_KEY", &secret_key);
    let cred_path = creds.save()?;

    println!(
        "{} folder `{}` -> schema `{}` · bucket `{}`",
        "trove init:".bold(),
        basename,
        schema.cyan(),
        bucket_name.cyan()
    );
    let bucket_state = BucketProbe {
        endpoint: bucket.clone(),
        access_key,
        secret_key,
    }
    .probe()?;

    if matches!(bucket_state, BucketState::Missing) {
        bail!(
            "bucket `{bucket_name}` is missing — create it in your R2 dashboard, then re-run `trove init`"
        );
    }

    let mut client = Client::connect(&versions_db, NoTls)
        .with_context(|| format!("connecting to {versions_db}"))?;
    client
        .batch_execute("create extension if not exists vector")
        .context("creating the pgvector extension")?;
    let schema_exists: bool = client
        .query_one("select to_regnamespace($1) is not null", &[&schema])?
        .get(0);

    match (schema_exists, bucket_state) {
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
        (_, BucketState::Missing) => unreachable!(),
    }

    let cache =
        provision::env_nonempty("TROVE_CACHE").unwrap_or_else(|| "/tmp/trove-cache".to_string());
    let mountpoint = cwd.canonicalize().unwrap_or(cwd);
    let vol_cfg = VolumeConfig {
        bucket: bucket.clone(),
        schema: schema.clone(),
        mountpoint: mountpoint.to_string_lossy().into_owned(),
        cache: cache.clone(),
    };
    let vol_path = vol_cfg.save(&volume)?;
    println!("{} wrote {}", "trove init:".bold(), cred_path.display());
    println!("{} wrote {}", "trove init:".bold(), vol_path.display());
    println!(
        "{} mounting `{volume}` at {}",
        "trove init:".bold(),
        mountpoint.display()
    );

    if opts.no_embed {
        println!("{} embedding disabled for this mount", "trove init:".bold());
    }

    Ok(InitMount {
        volume,
        schema,
        bucket,
        mountpoint,
        meta: versions_db.clone(),
        cache,
        versions_db,
    })
}

fn resolve_or_prompt(
    value: Option<String>,
    label: &str,
    current: Option<&str>,
    missing: &str,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    match prompt_if_tty(label, current) {
        Some(value) => value,
        None => Err(anyhow::anyhow!(missing.to_string())),
    }
}

fn resolve_secret_or_prompt(value: Option<String>, label: &str, missing: &str) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    match prompt_secret_if_tty(label) {
        Some(value) => value,
        None => Err(anyhow::anyhow!(missing.to_string())),
    }
}

fn prompt_if_tty(label: &str, current: Option<&str>) -> Option<Result<String>> {
    if !io::stdin().is_terminal() {
        return None;
    }
    let prompt = match current {
        Some(c) => format!("{label} [{c}]: "),
        None => format!("{label}: "),
    };
    Some(read_line(&prompt).and_then(|line| {
        let line = line.trim();
        if line.is_empty() {
            current
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{label} is required"))
        } else {
            Ok(line.to_string())
        }
    }))
}

fn prompt_secret_if_tty(label: &str) -> Option<Result<String>> {
    if !io::stdin().is_terminal() {
        return None;
    }
    Some(
        read_secret_line(&format!("{label} (hidden): ")).and_then(|line| {
            let line = line.trim().to_string();
            if line.is_empty() {
                Err(anyhow::anyhow!("{label} is required"))
            } else {
                Ok(line)
            }
        }),
    )
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line)
}

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
