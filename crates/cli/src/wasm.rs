//! `cli wasm strip-names <file.wasm>`: drops the `name` and `producers` custom sections from a
//! wasm module, in place.
//!
//! The `name` section is about a third of the hub bundle (docs/spikes/hub-size.md), and
//! `strip = true` cannot drop it with worker-build 0.8.7. Custom sections are self-contained
//! (no index refers to them), so removing one leaves every other byte, and every code offset,
//! as it was. The walker copies each kept section's bytes unchanged. Nothing else is rewritten.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};

/// `\0asm` followed by version 1.
const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
const CUSTOM_SECTION_ID: u8 = 0;
/// Custom sections removed by this step: debug names and the toolchain producers record.
const DROPPED_SECTIONS: [&str; 2] = ["name", "producers"];

const USAGE: &str = "usage: cli wasm strip-names <file.wasm>";

/// Entry point for `cli wasm <subcommand>`.
pub fn run(args: &[&str]) -> Result<()> {
    match args {
        ["strip-names", path] => {
            let (before, after) = strip_file(Path::new(path))?;
            eprintln!("strip-names {path}: {before} -> {after} bytes");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

/// Removes the `name` and `producers` custom sections from the wasm file at `path`, in place.
/// Returns the size before and after.
pub fn strip_file(path: &Path) -> Result<(usize, usize)> {
    let input = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let output = strip_names(&input).with_context(|| format!("strip {}", path.display()))?;
    std::fs::write(path, &output).with_context(|| format!("write {}", path.display()))?;
    Ok((input.len(), output.len()))
}

/// Returns `wasm` without its `name` and `producers` custom sections. Every other section is
/// copied byte for byte, including its id and size. Errors if `wasm` is not a version 1 module
/// or a section runs past the end of the input.
pub fn strip_names(wasm: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        wasm.starts_with(&HEADER),
        "not a wasm version 1 module (bad magic or version)"
    );
    let mut out = Vec::with_capacity(wasm.len());
    out.extend_from_slice(&HEADER);
    let mut pos = HEADER.len();
    while pos < wasm.len() {
        let start = pos;
        let id = wasm[pos];
        let (size, payload_start) =
            read_u32(wasm, pos + 1).with_context(|| format!("section at byte {start}: size"))?;
        let end = payload_start
            .checked_add(size as usize)
            .filter(|end| *end <= wasm.len())
            .ok_or_else(|| anyhow!("section at byte {start} runs past the end of the module"))?;
        let dropped = id == CUSTOM_SECTION_ID
            && DROPPED_SECTIONS.contains(&custom_name(&wasm[payload_start..end])?);
        if !dropped {
            out.extend_from_slice(&wasm[start..end]);
        }
        pos = end;
    }
    Ok(out)
}

/// The name at the start of a custom section payload.
fn custom_name(payload: &[u8]) -> Result<&str> {
    let (len, name_start) = read_u32(payload, 0).context("custom section: name length")?;
    let name = payload
        .get(name_start..name_start + len as usize)
        .ok_or_else(|| anyhow!("custom section: name runs past the section"))?;
    std::str::from_utf8(name).context("custom section: name is not UTF-8")
}

/// Reads an unsigned LEB128 `u32` at `pos`. Returns the value and the position after it.
fn read_u32(bytes: &[u8], pos: usize) -> Result<(u32, usize)> {
    let mut value: u64 = 0;
    for (index, byte) in bytes.iter().skip(pos).take(5).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            let value = u32::try_from(value).context("LEB128 value does not fit in u32")?;
            return Ok((value, pos + index + 1));
        }
    }
    bail!("truncated or over-long LEB128 at byte {pos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section with a payload under 128 bytes (one-byte size).
    fn section(id: u8, payload: &[u8]) -> Vec<u8> {
        assert!(
            payload.len() < 128,
            "test helper only encodes one-byte sizes"
        );
        let mut out = vec![id, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    fn custom(name: &str, data: &[u8]) -> Vec<u8> {
        let mut payload = vec![name.len() as u8];
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        section(CUSTOM_SECTION_ID, &payload)
    }

    fn module(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut wasm = HEADER.to_vec();
        for s in sections {
            wasm.extend_from_slice(s);
        }
        wasm
    }

    #[test]
    fn name_and_producers_sections_are_removed() {
        let kept = [
            section(1, &[0x01, 0x60, 0x00, 0x00]),
            section(3, &[0x01, 0x00]),
            custom("keep.me", &[0xaa, 0xbb]),
            section(10, &[0x01, 0x02, 0x00, 0x0b]),
        ];
        let mut sections = kept.to_vec();
        sections.insert(1, custom("name", &[0x01, 0x03, 0x61, 0x62, 0x63]));
        sections.push(custom("producers", &[0x01, 0x00]));
        let stripped = strip_names(&module(&sections)).unwrap();
        assert_eq!(stripped, module(&kept));
    }

    #[test]
    fn other_sections_are_byte_identical() {
        // A kept custom section and a code section with bytes that look like a name section.
        let code = section(10, &[0x01, 0x04, 0x00, 0x00, 0x0b, 0x00]);
        let other_custom = custom("name2", &[0x00, 0x00, 0x00]);
        let wasm = module(&[custom("name", &[0xff]), code.clone(), other_custom.clone()]);
        let stripped = strip_names(&wasm).unwrap();
        let mut expected = HEADER.to_vec();
        expected.extend_from_slice(&code);
        expected.extend_from_slice(&other_custom);
        assert_eq!(stripped, expected);
    }

    #[test]
    fn module_without_names_is_unchanged() {
        let wasm = module(&[section(1, &[0x00]), custom("other", &[0x01])]);
        assert_eq!(strip_names(&wasm).unwrap(), wasm);
    }

    #[test]
    fn stripping_twice_is_the_same_as_once() {
        let wasm = module(&[custom("name", &[0x01]), section(1, &[0x00])]);
        let once = strip_names(&wasm).unwrap();
        assert_eq!(strip_names(&once).unwrap(), once);
    }

    #[test]
    fn bad_magic_is_an_error() {
        let mut wasm = module(&[section(1, &[0x00])]);
        wasm[0] = 0x01;
        assert!(strip_names(&wasm).is_err());
        assert!(strip_names(&[0x00, 0x61, 0x73]).is_err());
    }

    #[test]
    fn section_past_the_end_is_an_error() {
        let mut wasm = module(&[section(1, &[0x00, 0x00, 0x00])]);
        wasm.truncate(wasm.len() - 1);
        assert!(strip_names(&wasm).is_err());
    }

    #[test]
    fn truncated_size_is_an_error() {
        let mut wasm = HEADER.to_vec();
        wasm.extend_from_slice(&[0x01, 0x80]);
        assert!(strip_names(&wasm).is_err());
    }

    #[test]
    fn non_utf8_custom_name_is_an_error() {
        let wasm = module(&[section(CUSTOM_SECTION_ID, &[0x01, 0xff])]);
        assert!(strip_names(&wasm).is_err());
    }

    #[test]
    fn custom_name_longer_than_section_is_an_error() {
        let wasm = module(&[section(CUSTOM_SECTION_ID, &[0x05, b'n'])]);
        assert!(strip_names(&wasm).is_err());
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert!(run(&["strip-names"]).is_err());
        assert!(run(&["bogus", "x"]).is_err());
    }
}
