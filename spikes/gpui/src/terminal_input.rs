//! Keyboard input for the terminal pane: a key press becomes the bytes a terminal sends to its
//! child process. Pure: no GPUI types, so the mapping is unit-tested without a window.

/// The bytes for one key press, or `None` when the key sends nothing (or is not mapped).
///
/// - `key` is GPUI's key name (`"enter"`, `"up"`, `"c"`, ...), `key_char` the text the key types
///   (`"A"` for shift-a), when there is one.
/// - Named keys use xterm's encodings (`CSI A` for up, `DEL` for backspace).
/// - `control` with a letter sends its control code (`ctrl-c` is `0x03`).
/// - `alt` prefixes printable text and named keys with `ESC`.
pub fn encode_key(key: &str, key_char: Option<&str>, control: bool, alt: bool) -> Option<Vec<u8>> {
    let named: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "backspace" => Some(b"\x7f"),
        "space" => Some(b" "),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "delete" => Some(b"\x1b[3~"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(with_alt(alt, bytes));
    }

    let mut chars = key.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };

    if control {
        // Ctrl-letter: the letter's position in the alphabet (a=1 .. z=26).
        let letter = single.filter(char::is_ascii_alphabetic)?;
        let code = letter.to_ascii_lowercase() as u8 - b'a' + 1;
        return Some(with_alt(alt, &[code]));
    }

    // Printable text: what the key types, else the key itself when it is one character.
    let text = key_char.or(single.map(|_| key))?;
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(with_alt(alt, text.as_bytes()))
}

fn with_alt(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::encode_key;

    #[test]
    fn terminal_input_enter_sends_carriage_return() {
        assert_eq!(
            encode_key("enter", None, false, false),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn terminal_input_arrows_send_csi_sequences() {
        assert_eq!(
            encode_key("up", None, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key("down", None, false, false),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_key("right", None, false, false),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            encode_key("left", None, false, false),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn terminal_input_ctrl_c_sends_etx() {
        assert_eq!(encode_key("c", None, true, false), Some(vec![0x03]));
        assert_eq!(encode_key("C", Some("C"), true, false), Some(vec![0x03]));
    }

    #[test]
    fn terminal_input_printable_text_uses_the_typed_character() {
        assert_eq!(
            encode_key("l", Some("l"), false, false),
            Some(b"l".to_vec())
        );
        assert_eq!(
            encode_key("a", Some("A"), false, false),
            Some(b"A".to_vec())
        );
        // No key_char: a single-character key name is the text.
        assert_eq!(encode_key("s", None, false, false), Some(b"s".to_vec()));
        assert_eq!(
            encode_key("é", Some("é"), false, false),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn terminal_input_backspace_tab_and_escape() {
        assert_eq!(
            encode_key("backspace", None, false, false),
            Some(vec![0x7f])
        );
        assert_eq!(encode_key("tab", None, false, false), Some(b"\t".to_vec()));
        assert_eq!(encode_key("escape", None, false, false), Some(vec![0x1b]));
    }

    #[test]
    fn terminal_input_alt_prefixes_escape() {
        assert_eq!(
            encode_key("b", Some("b"), false, true),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            encode_key("left", None, false, true),
            Some(b"\x1b\x1b[D".to_vec())
        );
    }

    #[test]
    fn terminal_input_unmapped_keys_send_nothing() {
        assert_eq!(encode_key("f13", None, false, false), None);
        assert_eq!(encode_key("shift", None, false, false), None);
        // Control with a non-letter has no code in this encoder.
        assert_eq!(encode_key("1", None, true, false), None);
        // A control character as key text is not printable input.
        assert_eq!(
            encode_key("enter", Some("\n"), false, false),
            Some(b"\r".to_vec())
        );
        assert_eq!(encode_key("x", Some("\u{7}"), false, false), None);
    }
}
