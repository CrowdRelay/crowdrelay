//! Rebuild this crate whenever a migration is added, removed or edited.
//!
//! `MIGRATOR` is `sqlx::migrate!("../../migrations")`, which embeds every
//! migration into the binary at compile time. Nothing about adding a file to
//! `migrations/` touches a source file in this crate, so cargo saw no reason to
//! recompile and the embedded set stayed stale — a test creating a fresh database
//! and calling `MIGRATOR.run` would silently migrate it to the previous
//! revision, then fail on whatever the new migration was supposed to have
//! created.
//!
//! `database.rs` documented that failure and prescribed `touch`ing itself as the
//! cure, which works and relies on the next person knowing. This makes cargo
//! responsible for it instead: one `rerun-if-changed` on the directory the macro
//! reads.
//!
//! Cost is a recompile of this crate when migrations change, which is exactly
//! when a recompile is correct.

use std::path::PathBuf;

fn main() {
    let migrations = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../migrations")
        .canonicalize()
        .expect("the workspace migrations directory must exist");
    // The directory itself catches additions and removals; cargo also walks it
    // for content changes, so an edited migration triggers a rebuild too.
    println!("cargo:rerun-if-changed={}", migrations.display());
}
