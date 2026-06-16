/// Python-compatible `str.splitlines()`.
///
/// Splits on the full universal-newline set:
///   \n (LF), \r (CR), \r\n (CRLF), \x0B (VT), \x0C (FF),
///   \x1C (FS), \x1D (GS), \x1E (RS), \u{85} (NEL), \u{2028} (LS), \u{2029} (PS)
///
/// `\r\n` is treated as a single boundary (not two).
/// No trailing empty element is produced (matches Python's `str.splitlines()`).
pub fn py_splitlines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return vec![];
    }

    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut line_start = 0;
    let mut i = 0;

    while i < len {
        // Determine if we're at a line boundary and its byte length.
        let boundary_len: Option<usize> = if bytes[i] == b'\r' {
            // \r\n counts as one boundary
            if i + 1 < len && bytes[i + 1] == b'\n' {
                Some(2)
            } else {
                Some(1)
            }
        } else if bytes[i] == b'\n'
            || bytes[i] == 0x0B  // VT
            || bytes[i] == 0x0C  // FF
            || bytes[i] == 0x1C  // FS
            || bytes[i] == 0x1D  // GS
            || bytes[i] == 0x1E
        // RS
        {
            Some(1)
        } else if bytes[i] == 0xC2 && i + 1 < len && bytes[i + 1] == 0x85 {
            // U+0085 NEL encoded as UTF-8: 0xC2 0x85
            Some(2)
        } else if bytes[i] == 0xE2
            && i + 2 < len
            && bytes[i + 1] == 0x80
            && (bytes[i + 2] == 0xA8 || bytes[i + 2] == 0xA9)
        {
            // U+2028 LS: 0xE2 0x80 0xA8
            // U+2029 PS: 0xE2 0x80 0xA9
            Some(3)
        } else {
            None
        };

        if let Some(blen) = boundary_len {
            result.push(&s[line_start..i]);
            i += blen;
            line_start = i;
        } else {
            // Advance by the length of the UTF-8 character.
            let ch_len = utf8_char_len(bytes[i]);
            i += ch_len;
        }
    }

    // Push the final segment only if it's non-empty (no trailing empty element).
    if line_start < len {
        result.push(&s[line_start..]);
    }

    result
}

/// Returns the byte length of a UTF-8 character given its leading byte.
#[inline]
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::py_splitlines;

    #[test]
    fn basic_lf() {
        assert_eq!(py_splitlines("a\nb\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn no_trailing_empty() {
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
    }

    #[test]
    fn crlf_and_cr() {
        assert_eq!(py_splitlines("a\r\nb\rc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn unicode_separators() {
        // U+2028 line separator, U+0085 NEL, vertical tab, form feed
        assert_eq!(
            py_splitlines("a\u{2028}b\u{0085}c\u{000b}d\u{000c}e"),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn empty() {
        assert!(py_splitlines("").is_empty());
    }

    #[test]
    fn file_separator_group_separator_record_separator() {
        assert_eq!(py_splitlines("a\x1cb\x1dc\x1ed"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn paragraph_separator() {
        assert_eq!(py_splitlines("a\u{2029}b"), vec!["a", "b"]);
    }

    #[test]
    fn no_trailing_empty_multiple() {
        // Multiple trailing newlines: Python returns lines without trailing empty
        assert_eq!(py_splitlines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn only_newline() {
        assert_eq!(py_splitlines("\n"), vec![""]);
    }
}
