//! `cli e2e <check>`: end-to-end checks.

pub mod app;
pub mod attachments;
pub mod tracer;

use anyhow::{Result, bail};

/// Runs the named end-to-end check.
pub async fn run(args: &[&str]) -> Result<()> {
    match args {
        ["tracer"] => tracer::run().await,
        ["attachments"] => attachments::run().await,
        ["app"] => app::run().await,
        // Not a check: the agent process the attachments check's stub harness starts.
        ["attachments-agent"] => attachments::agent().await,
        _ => bail!("usage: cli e2e tracer | cli e2e attachments | cli e2e app"),
    }
}
