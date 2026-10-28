//! Local MCP servers for a session (tunnel.md §3.5, SPEC-004): the runner's own configuration
//! until loadouts land. Read from Claude-style files, most specific first; the first entry with
//! a given name wins, as in Claude Code:
//! 1. `~/.claude.json` `projects[<project dir>].mcpServers` (local scope, where Claude Code
//!    keeps per-project servers);
//! 2. `<project dir>/.mcp.json` `mcpServers` (project scope);
//! 3. `~/.claude.json` top-level `mcpServers` (user scope).
//!
//! Each entry becomes an ACP `McpServer`:
//! - `stdio` (the default when `type` is missing): a bare `command` is resolved against the
//!   runner's `PATH` with the same executable check as discovery, because ACP requires an
//!   absolute path. An unresolvable command skips the server.
//! - `http` and `sse`: only when the agent advertises `mcp_capabilities.http` / `.sse`.
//!
//! Anything else, and any malformed entry, is skipped with a warning that names the server and
//! (for an unresolvable command) the command as written, never its env, args or values.
//! `${VAR}` and `${VAR:-default}` are expanded in command, args, env values, url and header
//! values from the runner environment. A missing `VAR` without a default expands to nothing.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Context;
use protocol::acp::{
    EnvVariable, HttpHeader, McpCapabilities, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use serde_json::Value;

/// The runner's environment: `PATH` resolves bare commands, every other entry feeds `${VAR}`.
pub type Env = BTreeMap<String, String>;

/// The process environment as an [`Env`], for the runner's own call. Non-UTF-8 bytes are lossy.
pub fn process_env() -> Env {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// The MCP servers for a session in `project_dir` that `caps` allows, sorted by name, with
/// `env` as the runner environment. Missing files are no servers; a file that exists but does
/// not parse is an error (its name, not its contents, is reported).
pub fn local_mcp_servers(
    home: &Path,
    project_dir: &Path,
    caps: &McpCapabilities,
    env: &Env,
) -> anyhow::Result<Vec<McpServer>> {
    let claude_json = read_json(&home.join(".claude.json"))?;
    let project_json = read_json(&project_dir.join(".mcp.json"))?;

    let local = claude_json
        .as_ref()
        .and_then(|v| v.get("projects"))
        .and_then(|p| p.get(project_dir.to_string_lossy().as_ref()))
        .and_then(|p| p.get("mcpServers"));
    let project = project_json.as_ref().and_then(|v| v.get("mcpServers"));
    let user = claude_json.as_ref().and_then(|v| v.get("mcpServers"));

    let mut seen = BTreeSet::new();
    let mut servers = Vec::new();
    for source in [local, project, user].into_iter().flatten() {
        let Some(entries) = source.as_object() else {
            continue;
        };
        for (name, entry) in entries {
            if !seen.insert(name.clone()) {
                continue;
            }
            match convert(name, entry, caps, env) {
                Some(server) => servers.push(server),
                None => tracing::warn!(server = %name, "skipping local MCP server"),
            }
        }
    }
    servers.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    Ok(servers)
}

fn read_json(path: &Path) -> anyhow::Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// One Claude-style entry. A missing `type` means stdio. `None` skips the server; the caller
/// logs the generic reason, and the cases with their own reason log it here, never with values.
fn convert(name: &str, entry: &Value, caps: &McpCapabilities, env: &Env) -> Option<McpServer> {
    let obj = entry.as_object()?;
    let kind = match obj.get("type") {
        None => "stdio",
        Some(v) => v.as_str()?,
    };
    match kind {
        "stdio" => {
            let raw = obj.get("command")?.as_str()?;
            let Some(command) = resolve_command(&expand(name, raw, env), env) else {
                tracing::warn!(
                    server = %name,
                    command = %raw,
                    "skipping local MCP server: command is not an absolute path or an executable on PATH"
                );
                return None;
            };
            let args: Vec<String> = string_list(obj.get("args"))?
                .iter()
                .map(|a| expand(name, a, env))
                .collect();
            let vars: Vec<EnvVariable> = string_map(obj.get("env"))?
                .into_iter()
                .map(|(k, v)| EnvVariable::new(k, expand(name, &v, env)))
                .collect();
            Some(McpServer::Stdio(
                McpServerStdio::new(name, command).args(args).env(vars),
            ))
        }
        "http" | "sse" => {
            let url = expand(name, obj.get("url")?.as_str()?, env);
            let headers: Vec<HttpHeader> = string_map(obj.get("headers"))?
                .into_iter()
                .map(|(k, v)| HttpHeader::new(k, expand(name, &v, env)))
                .collect();
            if kind == "http" {
                if !caps.http {
                    tracing::warn!(server = %name, "skipping local MCP server: the agent does not advertise http");
                    return None;
                }
                Some(McpServer::Http(
                    McpServerHttp::new(name, url).headers(headers),
                ))
            } else {
                if !caps.sse {
                    tracing::warn!(server = %name, "skipping local MCP server: the agent does not advertise sse");
                    return None;
                }
                Some(McpServer::Sse(
                    McpServerSse::new(name, url).headers(headers),
                ))
            }
        }
        _ => None,
    }
}

/// The absolute path to run for `command`. An absolute path is kept as it is. A bare name is
/// searched on `PATH` (absolute entries only) with the same executable check as discovery.
/// Any other relative path is unresolvable.
fn resolve_command(command: &str, env: &Env) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let path = Path::new(command);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    if path.components().count() != 1 {
        return None;
    }
    let path_env = env.get("PATH")?;
    std::env::split_paths(OsStr::new(path_env))
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(command))
        .find(|candidate| super::is_executable(candidate))
}

/// Expands `${VAR}` and `${VAR:-default}` from `env`. A `VAR` that is unset or empty takes the
/// default, or expands to nothing with a warning that names `VAR` (never its value). A reference
/// that is not well formed, or has no closing brace, is kept as written.
fn expand(server: &str, text: &str, env: &Env) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find('}') else {
            out.push_str(tail);
            return out;
        };
        let reference = &tail[..=end];
        rest = &tail[end + 1..];
        let body = &tail[2..end];
        let (var, default) = match body.split_once(":-") {
            Some((var, default)) => (var, Some(default)),
            None => (body, None),
        };
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push_str(reference);
            continue;
        }
        match env.get(var).filter(|v| !v.is_empty()) {
            Some(value) => out.push_str(value),
            None => match default {
                Some(default) => out.push_str(default),
                None => tracing::warn!(
                    server = %server,
                    var = %var,
                    "local MCP config references a variable that is not set; it expands to an empty string"
                ),
            },
        }
    }
    out.push_str(rest);
    out
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}

fn string_map(value: Option<&Value>) -> Option<Vec<(String, String)>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect(),
        Some(_) => None,
    }
}

fn name_of(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(s) => &s.name,
        McpServer::Http(h) => &h.name,
        McpServer::Sse(s) => &s.name,
        _ => "",
    }
}
