//! Local harness discovery, the spawn environment and local MCP passthrough (M2B-006).
//! Fake PATHs of temp dirs with stub executables: no real harness is needed.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use protocol::acp::McpServer;
use runner::harness::{self, Found};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Writes `body` to `dir/name` with `mode`.
fn write_file(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// An executable stub that does nothing.
fn stub(dir: &Path, name: &str) -> PathBuf {
    write_file(dir, name, "#!/bin/sh\nexit 0\n", 0o755)
}

fn path_env(dirs: &[&Path]) -> OsString {
    std::env::join_paths(dirs).unwrap()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// (harness, program file name, args) for each found harness, in order.
fn summary(found: &[Found]) -> Vec<(&'static str, String, Vec<String>)> {
    found
        .iter()
        .map(|f| {
            (
                f.harness,
                f.program
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                f.args.clone(),
            )
        })
        .collect()
}

fn all_adapters(dir: &Path) {
    for name in [
        "claude-agent-acp",
        "codex-acp",
        "opencode",
        "pi-acp",
        "gemini",
        "goose",
    ] {
        stub(dir, name);
    }
}

#[test]
fn none_found_when_path_is_empty() {
    let home = TempDir::new().unwrap();
    let found = harness::discover(OsStr::new(""), home.path());
    assert!(found.is_empty());
    assert!(harness::capabilities(&found, 32).harnesses.is_empty());
}

#[test]
fn finds_all_six_harnesses_with_their_acp_invocation() {
    let bin = TempDir::new().unwrap();
    all_adapters(bin.path());
    let home = TempDir::new().unwrap();

    let found = harness::discover(&path_env(&[bin.path()]), home.path());

    assert_eq!(
        summary(&found),
        vec![
            ("claude", "claude-agent-acp".to_owned(), strings(&[])),
            ("codex", "codex-acp".to_owned(), strings(&[])),
            ("opencode", "opencode".to_owned(), strings(&["acp"])),
            ("pi", "pi-acp".to_owned(), strings(&[])),
            (
                "gemini",
                "gemini".to_owned(),
                strings(&["--experimental-acp"])
            ),
            ("goose", "goose".to_owned(), strings(&["acp"])),
        ]
    );
    assert!(found.iter().all(|f| f.program.parent() == Some(bin.path())));
}

#[test]
fn cli_without_its_acp_adapter_is_not_a_harness() {
    // `claude` and `codex` are CLIs with no ACP mode of their own: the adapter is required.
    let bin = TempDir::new().unwrap();
    stub(bin.path(), "claude");
    stub(bin.path(), "codex");
    stub(bin.path(), "opencode");
    let home = TempDir::new().unwrap();

    let found = harness::discover(&path_env(&[bin.path()]), home.path());

    assert_eq!(
        summary(&found),
        vec![("opencode", "opencode".to_owned(), strings(&["acp"]))]
    );
}

#[test]
fn preferred_adapter_name_wins_within_a_harness() {
    let bin = TempDir::new().unwrap();
    stub(bin.path(), "claude-code-acp");
    stub(bin.path(), "claude-agent-acp");
    let home = TempDir::new().unwrap();

    let found = harness::discover(&path_env(&[bin.path()]), home.path());

    assert_eq!(
        summary(&found),
        vec![("claude", "claude-agent-acp".to_owned(), strings(&[]))]
    );
}

#[test]
fn first_path_directory_wins() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let first_goose = stub(first.path(), "goose");
    stub(second.path(), "goose");
    let home = TempDir::new().unwrap();

    let found = harness::discover(&path_env(&[first.path(), second.path()]), home.path());

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].program, first_goose);
}

#[test]
fn non_executable_file_is_ignored() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    write_file(first.path(), "goose", "#!/bin/sh\nexit 0\n", 0o644);
    let home = TempDir::new().unwrap();

    let none = harness::discover(&path_env(&[first.path()]), home.path());
    assert!(
        none.is_empty(),
        "a file without the execute bit is not a harness"
    );

    // The same name, executable, further down PATH is the one found.
    let real = stub(second.path(), "goose");
    let found = harness::discover(&path_env(&[first.path(), second.path()]), home.path());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].program, real);
}

#[test]
fn symlink_is_followed_and_dangling_symlink_is_ignored() {
    let target_dir = TempDir::new().unwrap();
    let link_dir = TempDir::new().unwrap();
    let target = stub(target_dir.path(), "goose");
    symlink(&target, link_dir.path().join("goose")).unwrap();
    // A dangling `opencode` link must not hide a real one further down PATH.
    symlink(
        target_dir.path().join("missing"),
        link_dir.path().join("opencode"),
    )
    .unwrap();
    let real_opencode = stub(target_dir.path(), "opencode");
    let home = TempDir::new().unwrap();

    let found = harness::discover(
        &path_env(&[link_dir.path(), target_dir.path()]),
        home.path(),
    );

    assert_eq!(
        summary(&found),
        vec![
            ("opencode", "opencode".to_owned(), strings(&["acp"])),
            ("goose", "goose".to_owned(), strings(&["acp"])),
        ]
    );
    assert_eq!(found[0].program, real_opencode);
    assert_eq!(found[1].program, link_dir.path().join("goose"));
}

#[test]
fn per_user_bin_dir_is_searched_after_path() {
    let home = TempDir::new().unwrap();
    let local_bin = home.path().join(".local/bin");
    let installed = stub(&local_bin, "claude-agent-acp");

    let found = harness::discover(OsStr::new(""), home.path());

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].program, installed);
}

#[test]
fn capabilities_match_the_hello_in_tunnel_spec() {
    let bin = TempDir::new().unwrap();
    stub(bin.path(), "claude-agent-acp");
    stub(bin.path(), "opencode");
    let home = TempDir::new().unwrap();
    let found = harness::discover(&path_env(&[bin.path()]), home.path());

    let caps = serde_json::to_value(harness::capabilities(&found, 32)).unwrap();

    // tunnel.md §3.1 hello, with `features` empty: the runner advertises nothing it does not
    // ship yet.
    assert_eq!(
        caps,
        json!({
            "host_mode": "local",
            "harnesses": ["claude", "opencode"],
            "max_sessions": 32,
            "spawn": ["same_dir", "worktree"],
            "features": []
        })
    );
}

#[test]
fn harness_names_are_in_table_order_whatever_order_discovery_gave() {
    let bin = TempDir::new().unwrap();
    all_adapters(bin.path());
    let home = TempDir::new().unwrap();
    let mut found = harness::discover(&path_env(&[bin.path()]), home.path());
    found.reverse();

    let caps = harness::capabilities(&found, 4);

    assert_eq!(
        caps.harnesses,
        ["claude", "codex", "opencode", "pi", "gemini", "goose"]
    );
    assert_eq!(caps.max_sessions, 4);
}

#[test]
fn spawn_inherits_the_environment_and_passes_the_acp_args() {
    // SAFETY: a fake key set once, before this test spawns anything; only this test reads it.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-test-inherit") };
    let dir = TempDir::new().unwrap();
    let script = write_file(
        dir.path(),
        "stub-harness",
        "#!/bin/sh\n\
         printf 'key=%s\\n' \"$ANTHROPIC_API_KEY\"\n\
         printf 'home=%s\\n' \"$HOME\"\n\
         printf 'path=%s\\n' \"$PATH\"\n\
         printf 'cwd=%s\\n' \"$(pwd -P)\"\n\
         printf 'args=%s\\n' \"$*\"\n",
        0o755,
    );
    let found = Found {
        harness: "opencode",
        program: script,
        args: strings(&["acp"]),
    };

    let mut cmd: Command = harness::spawn_command(&found, dir.path());
    // The key is inherited, not copied into the command.
    assert!(
        cmd.get_envs()
            .all(|(k, _)| k.to_str() != Some("ANTHROPIC_API_KEY"))
    );
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();

    let expected_home = std::env::var("HOME").unwrap_or_default();
    let expected_path = std::env::var("PATH").unwrap_or_default();
    let expected_cwd = fs::canonicalize(dir.path()).unwrap();
    assert!(stdout.contains("key=sk-test-inherit\n"), "{stdout}");
    assert!(
        stdout.contains(&format!("home={expected_home}\n")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("path={expected_path}\n")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("cwd={}\n", expected_cwd.display())),
        "{stdout}"
    );
    assert!(stdout.contains("args=acp\n"), "{stdout}");
}

fn server_json(server: &McpServer) -> Value {
    serde_json::to_value(server).unwrap()
}

#[test]
fn local_mcp_servers_merge_claude_sources_in_precedence_order() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let project_key = project.path().to_string_lossy().into_owned();
    fs::write(
        home.path().join(".claude.json"),
        json!({
            "mcpServers": {
                "fs": {"command": "/usr/bin/fs-mcp", "args": ["--root", "/home/u"], "env": {"FS_TOKEN": "user-level"}},
                "github": {"type": "http", "url": "https://mcp.example.test/mcp", "headers": {"Authorization": "Bearer hdr-value"}}
            },
            "projects": {
                project_key: {"mcpServers": {"local-only": {"command": "/opt/local-mcp", "args": []}}}
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        project.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "fs": {"command": "/work/fs-mcp"},
                "search": {"type": "stdio", "command": "/opt/search", "args": ["-v"], "env": {"SEARCH_KEY": "search-value"}},
                "legacy": {"type": "sse", "url": "https://sse.example.test"},
                "broken": {"command": 5}
            }
        })
        .to_string(),
    )
    .unwrap();

    let servers = runner::harness::local_mcp_servers(home.path(), project.path()).unwrap();

    let names: Vec<String> = servers
        .iter()
        .map(|s| server_json(s)["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["fs", "github", "local-only", "search"]);
    assert_eq!(
        server_json(&servers[0]),
        json!({"name": "fs", "command": "/work/fs-mcp", "args": [], "env": []}),
        "the project .mcp.json entry replaces the user-level one of the same name"
    );
    assert_eq!(
        server_json(&servers[1]),
        json!({
            "type": "http",
            "name": "github",
            "url": "https://mcp.example.test/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer hdr-value"}]
        })
    );
    assert_eq!(
        server_json(&servers[2]),
        json!({"name": "local-only", "command": "/opt/local-mcp", "args": [], "env": []})
    );
    assert_eq!(
        server_json(&servers[3]),
        json!({
            "name": "search",
            "command": "/opt/search",
            "args": ["-v"],
            "env": [{"name": "SEARCH_KEY", "value": "search-value"}]
        })
    );
}

#[test]
fn user_level_servers_come_through_when_the_project_has_none() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        home.path().join(".claude.json"),
        json!({"mcpServers": {"fs": {"command": "/usr/bin/fs-mcp", "env": {"FS_TOKEN": "user-level"}}}})
            .to_string(),
    )
    .unwrap();

    let servers = runner::harness::local_mcp_servers(home.path(), project.path()).unwrap();

    assert_eq!(
        server_json(&servers[0]),
        json!({
            "name": "fs",
            "command": "/usr/bin/fs-mcp",
            "args": [],
            "env": [{"name": "FS_TOKEN", "value": "user-level"}]
        })
    );
}

#[test]
fn missing_config_files_mean_no_servers() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let servers = runner::harness::local_mcp_servers(home.path(), project.path()).unwrap();
    assert!(servers.is_empty());
}

#[test]
fn malformed_config_is_an_error_that_does_not_show_its_values() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join(".mcp.json"),
        r#"{"mcpServers": {"x": {"env": {"KEY": "sekrit-value"}}"#,
    )
    .unwrap();

    let err = runner::harness::local_mcp_servers(home.path(), project.path()).unwrap_err();

    let text = format!("{err:#}");
    assert!(text.contains(".mcp.json"), "{text}");
    assert!(!text.contains("sekrit-value"), "{text}");
}
