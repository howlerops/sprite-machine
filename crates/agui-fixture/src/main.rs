//! `agui-fixture --scenario <name> [--rate <events/s>] [--addr <host:port>] [--thread <id>]`
//!
//! Serves a canned TurnEvent thread until Ctrl-C. Subscribe with
//! `ws://<addr>/threads/<thread>/subscribe?after=0`.

use std::net::SocketAddr;

use agui_fixture::{DEFAULT_THREAD_ID, FixtureConfig, Scenario, start};
use anyhow::{Context, Result, bail};

const USAGE: &str = "usage: agui-fixture --scenario <name> [--rate <events/s>] \
[--addr <host:port>] [--thread <id>]

scenarios:
  transcript-10k  about 10,000 events over many tasks
  diff-5k         one task writing a 5,000-line file as a chunked diff
  terminal        a terminal streaming output, then exiting
  approval        a run that waits for POST /threads/{id}/interrupts/int_00001

options:
  --rate <n>      events per second; 0 (default) appends as fast as possible
  --addr <a>      listen address (default 127.0.0.1:0, a free port)
  --thread <id>   thread id to serve (default th_fixture)";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(config) = parse_args(&args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let fixture = start(config).await?;
    println!(
        "agui-fixture serving thread {} on ws://{}/threads/{}/subscribe",
        fixture.thread_id(),
        fixture.addr(),
        fixture.thread_id()
    );
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;
    Ok(())
}

/// `Ok(None)` for `--help`.
fn parse_args(args: &[String]) -> Result<Option<FixtureConfig>> {
    let mut scenario: Option<Scenario> = None;
    let mut rate = 0.0_f64;
    let mut addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut thread_id = DEFAULT_THREAD_ID.to_owned();

    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        if flag == "--help" || flag == "-h" {
            return Ok(None);
        }
        let value = iter
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--scenario" => scenario = Some(value.parse().map_err(anyhow::Error::msg)?),
            "--rate" => {
                rate = value
                    .parse()
                    .with_context(|| format!("bad --rate {value}"))?
            }
            "--addr" => {
                addr = value
                    .parse()
                    .with_context(|| format!("bad --addr {value}"))?
            }
            "--thread" => thread_id = value.clone(),
            other => bail!("unknown option {other}\n\n{USAGE}"),
        }
    }
    let Some(scenario) = scenario else {
        bail!("--scenario is required\n\n{USAGE}");
    };
    Ok(Some(FixtureConfig {
        scenario,
        rate,
        addr,
        thread_id,
    }))
}
