//! The v1 auto-title (apps/control-plane/src/title.ts, ported): the first non-blank line of the
//! first prompt, with list, heading and quote markers stripped, cut to at most
//! [`MAX_TITLE_CHARS`] characters at a word boundary. Pure; the Thread core calls it when the
//! first `UserMessage` arrives, and only a title the client did not set uses it.

/// The longest auto-title, in characters (ellipsis included).
pub const MAX_TITLE_CHARS: usize = 80;

/// The auto-title for a prompt, or `None` when it has no text on any line.
pub fn derive_title(prompt: &str) -> Option<String> {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let stripped = strip_markers(first_line).trim();
    if stripped.is_empty() {
        return None;
    }
    Some(truncate_at_word(stripped, MAX_TITLE_CHARS))
}

/// Removes leading `#`-headings, `-`/`*`/`+`/`>` bullets and `1.`/`1)` numbers, repeatedly.
fn strip_markers(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        match strip_one_marker(trimmed) {
            Some(next) => rest = next,
            None => return trimmed,
        }
    }
}

/// One marker and the whitespace after it, or `None` when the text does not start with one.
fn strip_one_marker(text: &str) -> Option<&str> {
    let hashes = text.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes) {
        let after = &text[hashes..];
        if after.starts_with(char::is_whitespace) {
            return Some(after.trim_start());
        }
    }
    if let Some(first) = text.chars().next()
        && matches!(first, '-' | '*' | '+' | '>')
        && text[1..].starts_with(char::is_whitespace)
    {
        return Some(text[1..].trim_start());
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(after) = text[digits..].strip_prefix(['.', ')'])
        && after.starts_with(char::is_whitespace)
    {
        return Some(after.trim_start());
    }
    None
}

/// `text` if it has at most `max` characters; otherwise its first `max - 1` characters cut back
/// to the last whitespace (a hard cut when there is none), plus an ellipsis.
fn truncate_at_word(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let end = text
        .char_indices()
        .nth(max - 1)
        .map_or(text.len(), |(index, _)| index);
    let head = &text[..end];
    let cut = match head.rfind(char::is_whitespace) {
        Some(index) if index > 0 => head[..index].trim_end(),
        _ => head,
    };
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_non_blank_line_is_the_title() {
        assert_eq!(
            derive_title("\n\n  Upgrade axum to 0.8  \nmore detail").as_deref(),
            Some("Upgrade axum to 0.8")
        );
    }

    #[test]
    fn markdown_markers_are_stripped() {
        assert_eq!(
            derive_title("# Plan the release").as_deref(),
            Some("Plan the release")
        );
        assert_eq!(derive_title("- fix login").as_deref(), Some("fix login"));
        assert_eq!(
            derive_title("> 2) write tests").as_deref(),
            Some("write tests")
        );
        // A marker without its space is text, not a marker.
        assert_eq!(derive_title("#hashtag").as_deref(), Some("#hashtag"));
    }

    #[test]
    fn a_long_line_is_cut_at_a_word_boundary_within_80_characters() {
        let prompt = "word ".repeat(40);
        let title = derive_title(&prompt).expect("title");
        assert!(title.chars().count() <= MAX_TITLE_CHARS, "{title}");
        assert!(title.ends_with('…'), "{title}");
        let body = title.trim_end_matches('…');
        assert!(
            body.split_whitespace().all(|word| word == "word"),
            "{title}"
        );
    }

    #[test]
    fn a_line_without_spaces_is_cut_hard() {
        let title = derive_title(&"x".repeat(200)).expect("title");
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn a_blank_prompt_has_no_title() {
        assert_eq!(derive_title(""), None);
        assert_eq!(derive_title("  \n \n"), None);
        // A bare `#` is text, as in the v1 heuristic (its marker needs whitespace after it).
        assert_eq!(derive_title("# ").as_deref(), Some("#"));
    }
}
