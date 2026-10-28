//! Local MCP servers for a session (tunnel.md §3.5, SPEC-004): the runner's own configuration
//! until loadouts land. Read from Claude-style files, most specific first; the first entry with
//! a given name wins, as in Claude Code:
//! 1. `~/.claude.json` `projects[<project dir>].mcpServers` (local scope, where Claude Code
//!    keeps per-project servers);
//! 2. `<project dir>/.mcp.json` `mcpServers` (project scope);
//! 3. `~/.claude.json` top-level `mcpServers` (user scope).
//!
//! Only `stdio` and `http` entries are converted (the ACP `McpServer` variants this runner
//! passes on). `sse` entries and malformed ones are skipped with a warning that names the
//! server, never its values. `${VAR}` expansion is not done. Env values and headers are passed
//! through unchanged and are never logged.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use protocol::acp::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio};
use serde_json::Value;

/// The MCP servers for a session in `project_dir`, sorted by name. Missing files are no servers;
/// a file that exists but does not parse is an error (its name, not its contents, is reported).
pub fn local_mcp_servers(home: &Path, project_dir: &Path) -> anyhow::Result<Vec<McpServer>> {
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
            match convert(name, entry) {
                Some(server) => servers.push(server),
                None => tracing::warn!(
                    server = %name,
                    "skipping local MCP server: only stdio and http entries with string fields are supported"
                ),
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

/// One Claude-style entry. A missing `type` means stdio.
fn convert(name: &str, entry: &Value) -> Option<McpServer> {
    let obj = entry.as_object()?;
    let kind = match obj.get("type") {
        None => "stdio",
        Some(v) => v.as_str()?,
    };
    match kind {
        "stdio" => {
            let command = obj.get("command")?.as_str()?;
            let args = string_list(obj.get("args"))?;
            let env = string_map(obj.get("env"))?
                .into_iter()
                .map(|(k, v)| EnvVariable::new(k, v))
                .collect();
            Some(McpServer::Stdio(
                McpServerStdio::new(name, command).args(args).env(env),
            ))
        }
        "http" => {
            let url = obj.get("url")?.as_str()?;
            let headers = string_map(obj.get("headers"))?
                .into_iter()
                .map(|(k, v)| HttpHeader::new(k, v))
                .collect();
            Some(McpServer::Http(
                McpServerHttp::new(name, url).headers(headers),
            ))
        }
        _ => None,
    }
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
        _ => "",
    }
}
