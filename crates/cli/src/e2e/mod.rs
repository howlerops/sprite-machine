//! `cli e2e <check>`: end-to-end checks.

pub mod tracer;

use anyhow::{Result, bail};

/// Runs the named end-to-end check.
pub async fn run(args: &[&str]) -> Result<()> {
    match args {
        ["tracer"] => tracer::run().await,
        _ => bail!("usage: cli e2e tracer"),
    }
}
