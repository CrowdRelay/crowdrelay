//! Auto-discovers the latest migration number at compile time so
//! `SCHEMA_VERSION` in `meta.rs` never needs a manual bump.
//!
//! Scans the workspace `migrations/` directory for `NNNN_*.sql` files,
//! takes the highest prefix, and sets it as `CROWDRELAY_SCHEMA_VERSION`.
//! The `cargo:rerun-if-changed` directive ensures a rebuild whenever
//! migrations are added or removed.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by Cargo"),
    );

    // migrations/ lives at the workspace root, two levels up from
    // crates/crowdrelay-api/.
    let migrations_dir = manifest_dir.join("../../migrations");

    let latest = read_latest_migration_number(&migrations_dir).unwrap_or_else(|| {
        panic!(
            "no migration files found in {} — cannot determine SCHEMA_VERSION",
            migrations_dir.display()
        )
    });

    println!("cargo:rerun-if-changed=../../migrations");
    println!("cargo:rustc-env=CROWDRELAY_SCHEMA_VERSION={latest}");
}

/// Reads the `migrations/` directory and returns the highest `NNNN` prefix.
fn read_latest_migration_number(migrations_dir: &PathBuf) -> Option<u32> {
    let entries = std::fs::read_dir(migrations_dir).ok()?;
    let mut max: Option<u32> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        // Migration files are `NNNN_descriptive_name.sql`.
        if !name.ends_with(".sql") {
            continue;
        }
        let prefix = name.split('_').next()?;
        if prefix.len() != 4 || !prefix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let num: u32 = prefix.parse().ok()?;
        max = Some(max.map_or(num, |m| m.max(num)));
    }
    max
}
