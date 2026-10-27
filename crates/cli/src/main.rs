//! Developer CLI: end-to-end checks such as `e2e tracer`.
//!
//! ```text
//! cargo run -p cli -- e2e tracer
//! ```

mod e2e;

use anyhow::{Result, bail};

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

const USAGE: &str = "usage: cli e2e tracer";

#[tokio::main]
async fn main() -> Result<()> {
    // Warnings from the tracer stubs (dropped commands, nacks) go to stderr.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["e2e", rest @ ..] => e2e::run(rest).await,
        ["--version"] => {
            println!("{CRATE} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "cli");
    }
}
