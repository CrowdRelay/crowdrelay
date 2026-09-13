//! The worker binary's command line: what `crowdrelay-worker` accepts and how a
//! string of arguments becomes a `Command`.
//!
//! Split out of `main.rs` to keep it inside the source-size ratchet, and because
//! argument parsing is the one part of the binary with no runtime dependencies —
//! it is pure enough to test without a database, and mixing it into the file that
//! also owns the supervision tree made both harder to read.

use anyhow::{Result, bail};
use crowdrelay_worker::replay::{ReplayOptions, parse_replay_options};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Run {
        standby: bool,
    },
    Migrate,
    Bootstrap,
    Setup,
    Replay(ReplayOptions),
    /// Bulk-import hand-curated outreach contacts from a CSV.
    ImportOutreach {
        path: std::path::PathBuf,
    },
    ImportOpportunities {
        path: std::path::PathBuf,
    },
}

impl Command {
    pub(crate) const KNOWN: &'static str = "`run`, `run --standby`, `migrate`, `bootstrap`, `setup`, `replay`, `import-outreach <csv>`, or `import-opportunities <csv>`";
}

pub(crate) fn parse_command(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter();
    let head = args.next();
    let rest: Vec<String> = args.collect();
    let command = match head.as_deref() {
        None | Some("run") => {
            let mut standby = parse_standby_flag(&rest)?;
            if !standby && std::env::var("CROWDRELAY_WORKER_STANDBY").as_deref() == Ok("true") {
                standby = true;
            }
            Command::Run { standby }
        }
        Some("migrate") => {
            reject_extras(&rest)?;
            Command::Migrate
        }
        Some("bootstrap") => {
            reject_extras(&rest)?;
            Command::Bootstrap
        }
        Some("setup") => {
            reject_extras(&rest)?;
            Command::Setup
        }
        Some("replay") => Command::Replay(parse_replay_options(rest)?),
        Some("import-opportunities") => {
            let Some((path, extras)) = rest.split_first() else {
                bail!("import-opportunities needs a CSV path");
            };
            if let Some(extra) = extras.first() {
                bail!("unexpected worker argument `{extra}`");
            }
            Command::ImportOpportunities {
                path: std::path::PathBuf::from(path),
            }
        }
        Some("import-outreach") => {
            let Some((path, extras)) = rest.split_first() else {
                bail!("import-outreach needs a CSV path");
            };
            if let Some(extra) = extras.first() {
                bail!("unexpected worker argument `{extra}`");
            }
            Command::ImportOutreach {
                path: std::path::PathBuf::from(path),
            }
        }
        Some(other) => bail!(
            "unknown worker command `{other}`; expected {}",
            Command::KNOWN
        ),
    };

    Ok(command)
}

fn reject_extras(rest: &[String]) -> Result<()> {
    if let Some(extra) = rest.first() {
        bail!("unexpected worker argument `{extra}`");
    }
    Ok(())
}

fn parse_standby_flag(rest: &[String]) -> Result<bool> {
    let mut standby = false;
    for arg in rest {
        match arg.as_str() {
            "--standby" => standby = true,
            other => bail!("unexpected `run` argument `{other}`; expected `--standby`"),
        }
    }
    Ok(standby)
}
