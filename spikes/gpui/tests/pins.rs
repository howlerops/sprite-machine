//! The spike must build against exactly the GPUI pins of the root `[workspace.dependencies]`.

use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// The value of `key = "…"` on the first line that starts with `dep =` (or `dep.`).
fn spec_value(manifest: &str, dep: &str, key: &str) -> String {
    let line = manifest
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&format!("{dep} =")))
        .unwrap_or_else(|| panic!("no `{dep} =` line"));
    let needle = format!("{key} = \"");
    let start = line
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key} in `{line}`"))
        + needle.len();
    let end = start + line[start..].find('"').expect("closing quote");
    line[start..end].to_owned()
}

#[test]
fn gpui_pins_match_the_root_workspace() {
    let root = read("../../Cargo.toml");
    let spike = read("Cargo.toml");
    for dep in ["gpui-component", "gpui-kit"] {
        assert_eq!(
            spec_value(&spike, dep, "git"),
            spec_value(&root, dep, "git")
        );
        let rev = spec_value(&root, dep, "rev");
        assert_eq!(spec_value(&spike, dep, "rev"), rev, "{dep} rev");
        assert_eq!(rev.len(), 40, "{dep} is pinned to a full commit");
    }
    assert_eq!(
        spec_value(&spike, "gpui", "package"),
        spec_value(&root, "gpui", "package")
    );
    assert_eq!(
        spec_value(&spike, "gpui", "version"),
        spec_value(&root, "gpui", "version")
    );
}

#[test]
fn lockfiles_resolve_the_same_gpui_sources() {
    let root = read("../../Cargo.lock");
    let spike = read("Cargo.lock");
    let rev = spec_value(&read("../../Cargo.toml"), "gpui-kit", "rev");
    let source = format!("git+https://github.com/longbridge/gpui-component?rev={rev}#{rev}");
    assert!(root.contains(&source), "root lock resolves {source}");
    assert!(spike.contains(&source), "spike lock resolves {source}");
    let gpui_pre = "name = \"gpui-pre\"\nversion = \"0.3.10\"";
    assert!(root.contains(gpui_pre) && spike.contains(gpui_pre));
}
