//! Local harness discovery (PLAN-v2-ade.md §4.10 "Local environment"): which coding harnesses
//! this machine can run in ACP mode, the command that starts each one, and the hello
//! `capabilities` they add up to (tunnel.md §3.1).
//!
//! The spawn command inherits the runner's environment (logins such as `ANTHROPIC_API_KEY`
//! included). Nothing here copies, stores or logs credentials.
//!
//! - [`HARNESSES`]: the table of ACP entry points, data only;
//! - [`discover`]: finds them on `PATH`;
//! - [`capabilities`]: the hello `Capabilities` for what was found;
//! - [`spawn_command`]: the command that starts one in ACP mode;
//! - [`mcp`]: the local MCP servers a session is given.

pub mod mcp;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use protocol::tunnel::Capabilities;

pub use self::mcp::local_mcp_servers;

/// One way to start a harness in ACP mode: an executable name and its fixed arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// A harness and its ACP entry points, in preference order. The first one found wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Harness {
    pub name: &'static str,
    pub acp: &'static [Entry],
}

/// The ACP entry points, in the order the hello reports them (PLAN §4.10 and the list on
/// PLAN line 388). A new harness, or a changed invocation, is a row here.
///
/// Checked on 2026-10-29 (M2B-006a): each invocation below answered an ACP `initialize` with
/// `protocolVersion: 1`, except goose, which is still unconfirmed.
/// - claude: `claude-agent-acp` (npm `@agentclientprotocol/claude-agent-acp` 0.99.0, also published
///   as `@zed-industries/claude-agent-acp`); `claude-code-acp` (`@zed-industries/claude-code-acp`
///   0.16.2) is the older one.
/// - codex: `codex-acp` (npm `@zed-industries/codex-acp` 0.16.0).
/// - opencode: native `opencode acp` (npm `opencode-ai` 1.19.5).
/// - pi: `pi-acp` (npm `pi-acp` 0.0.34) or `pi-acpinator` (npm `pi-acpinator` 0.1.0).
/// - gemini: `gemini --acp` (npm `@google/gemini-cli` 0.66.0, where `--experimental-acp` is a
///   deprecated alias).
/// - goose: `goose acp`, not yet checked against a real binary.
pub const HARNESSES: &[Harness] = &[
    Harness {
        name: "claude",
        acp: &[
            Entry {
                program: "claude-agent-acp",
                args: &[],
            },
            Entry {
                program: "claude-code-acp",
                args: &[],
            },
        ],
    },
    Harness {
        name: "codex",
        acp: &[Entry {
            program: "codex-acp",
            args: &[],
        }],
    },
    Harness {
        name: "opencode",
        acp: &[Entry {
            program: "opencode",
            args: &["acp"],
        }],
    },
    Harness {
        name: "pi",
        acp: &[
            Entry {
                program: "pi-acp",
                args: &[],
            },
            Entry {
                program: "pi-acpinator",
                args: &[],
            },
        ],
    },
    Harness {
        name: "gemini",
        acp: &[Entry {
            program: "gemini",
            args: &["--acp"],
        }],
    },
    Harness {
        name: "goose",
        acp: &[Entry {
            program: "goose",
            args: &["acp"],
        }],
    },
];

/// A harness found on this machine: the command and arguments that start it in ACP mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub harness: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Finds each harness of [`HARNESSES`] on `path_env`, then in `home/.local/bin` (the per-user
/// bin dir the Claude and uv installers use). Within a harness, the first candidate in
/// preference order wins; for one candidate, the first directory wins. Symlinks count as their
/// target, and a file must have an execute bit to count. Empty `PATH` entries are skipped.
/// The result is in table order.
pub fn discover(path_env: &OsStr, home: &Path) -> Vec<Found> {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path_env)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    if !home.as_os_str().is_empty() {
        dirs.push(home.join(".local/bin"));
    }
    HARNESSES
        .iter()
        .filter_map(|harness| {
            harness.acp.iter().find_map(|entry| {
                dirs.iter()
                    .map(|dir| dir.join(entry.program))
                    .find(|path| is_executable(path))
                    .map(|program| Found {
                        harness: harness.name,
                        program,
                        args: entry.args.iter().map(|arg| (*arg).to_owned()).collect(),
                    })
            })
        })
        .collect()
}

/// The hello `Capabilities` (tunnel.md §3.1) for `found`: harness names in table order whatever
/// order `found` is in, the spawn modes `sm rc` supports, and `max_sessions`.
pub fn capabilities(found: &[Found], max_sessions: u32) -> Capabilities {
    let harnesses = HARNESSES
        .iter()
        .filter(|harness| found.iter().any(|f| f.harness == harness.name))
        .map(|harness| harness.name.to_owned())
        .collect();
    Capabilities {
        host_mode: "local".to_owned(),
        harnesses,
        max_sessions,
        spawn: vec!["same_dir".to_owned(), "worktree".to_owned()],
        features: Vec::new(),
    }
}

/// The command that starts `found` in ACP mode in `cwd`. The environment is inherited as it is:
/// nothing is cleared and nothing is copied in, so credentials such as `ANTHROPIC_API_KEY` reach
/// the harness and never appear in a config file or a log line.
pub fn spawn_command(found: &Found, cwd: &Path) -> Command {
    let mut cmd = Command::new(&found.program);
    cmd.args(&found.args).current_dir(cwd);
    cmd
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // `metadata` follows symlinks; a dangling one fails here and is skipped.
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}
