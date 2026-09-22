//! Making text from other people safe to print in a terminal.
//!
//! Everything an agent, a workflow or a server says is untrusted: a control sequence in it can
//! move the cursor, recolour or clear the screen, or change the window title. Strip them all.

/// Characters that reorder or hide text without printing anything themselves (bidirectional
/// overrides and isolates, and the zero-width and BOM family).
fn is_invisible_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Keep printable text and line breaks and tabs; drop every other control character (ESC and so
/// every ANSI/OSC sequence's introducer, the C1 range, DEL) and the invisible formatting
/// characters. `\r` is dropped so a carriage return cannot overwrite what was printed before it.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| matches!(c, '\n' | '\t') || !(c.is_control() || is_invisible_format(*c)))
        .collect()
}

/// For values that must be one unbroken token (a code, an address): every control and invisible
/// character is removed, line breaks and tabs included.
pub fn strip_controls(text: &str) -> String {
    text.chars()
        .filter(|c| !(c.is_control() || is_invisible_format(*c)))
        .collect()
}

/// As [`sanitize`], for text that must stay on one line: line breaks and tabs become spaces.
pub fn sanitize_line(text: &str) -> String {
    sanitize(&text.replace(['\n', '\t'], " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_including_unicode_line_breaks_and_tabs_is_untouched() {
        let text = "Hello, world!\n\tindented ✓ 日本語 émoji 😀\nlast line";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn escape_sequences_cannot_reach_the_terminal() {
        // Recolour, clear the screen, move the cursor, set the window title, ring the bell.
        for hostile in [
            "\u{1b}[31mred\u{1b}[0m",
            "\u{1b}[2J\u{1b}[H",
            "\u{1b}]0;pwned\u{7}",
            "\u{7}",
            "a\u{8}b",
            "\u{9b}31m",
            "x\u{7f}y",
        ] {
            let cleaned = sanitize(hostile);
            assert!(
                !cleaned
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "{hostile:?} -> {cleaned:?}"
            );
        }
        assert_eq!(
            sanitize("\u{1b}[31mred\u{1b}[0m"),
            "[31mred[0m",
            "the introducer goes; what is left is inert text"
        );
    }

    #[test]
    fn a_carriage_return_cannot_overwrite_earlier_output() {
        assert_eq!(sanitize("approve\rdeny  "), "approvedeny  ");
        assert_eq!(sanitize("a\r\nb"), "a\nb");
    }

    #[test]
    fn invisible_reordering_characters_are_removed() {
        for c in [
            '\u{202E}', '\u{202D}', '\u{2066}', '\u{2069}', '\u{200B}', '\u{FEFF}', '\u{200F}',
        ] {
            assert_eq!(sanitize(&format!("a{c}b")), "ab", "{:?}", c);
        }
        // A right-to-left override is how "cod\u{202E}txt.exe" pretends to be another name.
        assert_eq!(sanitize("cod\u{202E}txt.exe"), "codtxt.exe");
    }

    #[test]
    fn a_token_loses_every_control_and_break() {
        assert_eq!(strip_controls("BCDF-GHJK\u{1b}[2J\r\n\t"), "BCDF-GHJK[2J");
        assert_eq!(strip_controls("https://x/\u{202E}y"), "https://x/y");
    }

    #[test]
    fn one_line_text_has_no_breaks_at_all() {
        assert_eq!(sanitize_line("a\nb\tc\r\nd\u{1b}[0m"), "a b c d[0m");
    }
}
