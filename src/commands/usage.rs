//! `trove usage` — a quick "is this growing the way I expect?" report.
//!
//! Three sections: **Postgres** (DB size + per-table rows/bytes), **JuiceFS
//! volume** (what JuiceFS thinks is sitting in the bucket — same view the
//! application has, since `statvfs` reports the bill-shaped figure after
//! chunking and dedup), and **Content** (distinct paths + embedding progress).
//!
//! The number worth watching is `pending embedding` in the Content section.
//! Climbing pending + flat embedded = the embedder isn't keeping up (check
//! `OPENAI_API_KEY`, kick `trove embed --watch`).
//!
//! Mount-feature only — needs both the version DB and a JuiceFS handle.

use anyhow::Result;
use colored::Colorize;

use crate::jfs::Fs;
use crate::version::{DbUsage, VersionStore};

/// A single snapshot the printer can format. Carries DB and volume figures
/// together so the report is one cohesive view (don't print half if the other
/// half fails).
pub struct UsageReport {
    pub db: DbUsage,
    pub volume_total_bytes: u64,
    pub volume_available_bytes: u64,
}

/// Gather a `UsageReport`: one read-only DB transaction + one `statvfs` call.
pub fn run(fs: &Fs, versions: &mut VersionStore) -> Result<UsageReport> {
    let db = versions.usage()?;
    let (volume_total_bytes, volume_available_bytes) = fs.statvfs()?;
    Ok(UsageReport {
        db,
        volume_total_bytes,
        volume_available_bytes,
    })
}

/// Print the report. Three sections, fixed-width right-aligned numbers so
/// figures scan vertically. Section headings bold; trailing notes dimmed.
pub fn print(report: &UsageReport) {
    println!("{}\n", "trove usage".bold());

    // --- Postgres ---
    println!("  {}", "Postgres".bold());
    row("database total", &human_bytes_u(report.db.database_bytes), "");
    rows_bytes("blobs", report.db.blobs_rows, report.db.blobs_bytes, "");
    rows_bytes(
        "file_versions",
        report.db.file_versions_rows,
        report.db.file_versions_bytes,
        "",
    );
    rows_bytes(
        "blob_chunks",
        report.db.blob_chunks_rows,
        report.db.blob_chunks_bytes,
        "(embeddings)",
    );
    println!();

    // --- JuiceFS volume ---
    let used = report
        .volume_total_bytes
        .saturating_sub(report.volume_available_bytes);
    println!("  {}", "JuiceFS volume (file data \u{2192} bucket)".bold());
    row(
        "volume total",
        &human_bytes(report.volume_total_bytes),
        "",
    );
    row("volume used", &human_bytes(used), "(chunks in R2)");
    row(
        "volume available",
        &human_bytes(report.volume_available_bytes),
        "",
    );
    println!();

    // --- Content ---
    println!("  {}", "Content".bold());
    row(
        "distinct paths",
        &human_count(report.db.distinct_paths),
        "files",
    );
    row(
        "embedded blobs",
        &human_count(report.db.embedded_blobs),
        "",
    );
    let pending_note = if report.db.pending_blobs > 0 {
        "(run `trove embed`)"
    } else {
        ""
    };
    row(
        "pending embedding",
        &human_count(report.db.pending_blobs),
        pending_note,
    );
}

/// One row of the report: `  <label-left>  <value-right>   <dim-note>`.
/// Label padded to 22, value right-aligned in 12 so 3-digit GBs line up with
/// 3-digit MBs.
fn row(label: &str, value: &str, note: &str) {
    let line = format!("    {:<22}{:>12}", label, value);
    if note.is_empty() {
        println!("{line}");
    } else {
        println!("{line}    {}", note.dimmed());
    }
}

/// "<count> rows · <bytes>" row. Used for table sizing so rows and bytes sit
/// side-by-side without breaking the 12-wide value column (the count goes in
/// the value slot, bytes in the note slot).
fn rows_bytes(label: &str, rows: i64, bytes: i64, suffix: &str) {
    let note = if suffix.is_empty() {
        format!("rows \u{B7} {}", human_bytes_u(bytes))
    } else {
        format!("rows \u{B7} {}    {}", human_bytes_u(bytes), suffix)
    };
    row(label, &format!("{}", human_count(rows)), &note);
}

/// Decimal-prefix byte formatter: "17.2 MB" / "2.4 GB" / "412 KB" / "950 B".
/// Users expect "1 MB" to mean a million bytes — this matches that.
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{:.1} {}", v, UNITS[i])
}

/// `human_bytes` over an `i64` (DB counts come back as `bigint`). Negative
/// values shouldn't be possible from Postgres size functions, but if one ever
/// leaks through we render it as "0 B" rather than panicking.
fn human_bytes_u(n: i64) -> String {
    if n < 0 {
        "0 B".to_string()
    } else {
        human_bytes(n as u64)
    }
}

/// Group-of-three thousands separator for row counts: 1234 -> "1,234". `i64`
/// because `count(*)` is `bigint`. Negative values are rendered with the sign
/// preserved.
pub fn human_count(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    // Walk the digits right-to-left, inserting a comma every 3.
    let mut rev = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            rev.push(',');
        }
        rev.push(c);
    }
    let mut out = String::with_capacity(rev.len() + 1);
    if negative {
        out.push('-');
    }
    out.extend(rev.chars().rev());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_small() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
    }

    #[test]
    fn human_bytes_kb() {
        assert_eq!(human_bytes(1_000), "1.0 KB");
        assert_eq!(human_bytes(1_500), "1.5 KB");
    }

    #[test]
    fn human_bytes_mb() {
        assert_eq!(human_bytes(1_000_000), "1.0 MB");
        assert_eq!(human_bytes(17_200_000), "17.2 MB");
    }

    #[test]
    fn human_bytes_gb_and_above() {
        assert_eq!(human_bytes(2_400_000_000), "2.4 GB");
        assert_eq!(human_bytes(250_000_000_000), "250.0 GB");
    }

    #[test]
    fn human_count_formats_with_commas() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(7), "7");
        assert_eq!(human_count(42), "42");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_000), "1,000");
        assert_eq!(human_count(1_234), "1,234");
        assert_eq!(human_count(12_345), "12,345");
        assert_eq!(human_count(123_456), "123,456");
        assert_eq!(human_count(1_234_567), "1,234,567");
    }

    #[test]
    fn human_count_handles_negatives() {
        assert_eq!(human_count(-1_234), "-1,234");
    }

    #[test]
    fn human_bytes_u_clamps_negative_to_zero() {
        assert_eq!(human_bytes_u(-1), "0 B");
        assert_eq!(human_bytes_u(0), "0 B");
        assert_eq!(human_bytes_u(17_200_000), "17.2 MB");
    }
}
