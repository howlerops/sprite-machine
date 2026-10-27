//! `acp-mock --script <path.json>`: serves a scripted ACP agent on stdin and stdout.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut script_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--script" => {
                let path = args.next().context("--script needs a path")?;
                script_path = Some(PathBuf::from(path));
            }
            other => bail!("unknown argument {other:?}; usage: acp-mock --script <path.json>"),
        }
    }
    let path = script_path.context("usage: acp-mock --script <path.json>")?;
    let script = acp_mock::Script::from_path(&path)?;
    acp_mock::run(script, tokio::io::stdin(), tokio::io::stdout()).await
}
