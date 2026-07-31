// SPDX-License-Identifier: Apache-2.0

//! Terminal sanitation for hub-sourced text.
//!
//! Every string a hub authors — record bodies, slugs, names, grant fields,
//! error messages — is untrusted terminal input: embedded ANSI/C0 control
//! sequences could recolor, retitle, or spoof the operator's terminal. TEXT
//! output therefore routes hub-sourced strings through [`sanitize`] before
//! printing. `--json` output is never sanitized: it is a machine surface,
//! JSON string encoding already neutralizes control bytes (`\u001b`), and the
//! consumer gets the hub's bytes verbatim.

/// Strip terminal control content from one hub-sourced string: ANSI/VT escape
/// sequences (`ESC [ … m`, `ESC ] … BEL`, two-character escapes) are removed
/// whole, and every other control character — C0 except `\n` and `\t`, DEL,
/// and the C1 range — is dropped. Printable text, including non-ASCII, passes
/// through untouched.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // An escape sequence: swallow it whole.
            match chars.peek() {
                // CSI: `ESC [` params/intermediates, then a final byte in @–~.
                Some('[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: `ESC ]` … terminated by BEL or ST (`ESC \`).
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character escapes (`ESC c`, `ESC 7`, …): drop the pair.
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if !is_directional_control(c) && (!c.is_control() || c == '\n' || c == '\t') {
            out.push(c);
        }
        // Any other control char (C0, DEL, C1) is dropped.
    }
    out
}

/// Sanitize one terminal field that must not create another visual line or
/// column. Newlines and tabs are rendered visibly; ANSI, C0/C1, and Unicode
/// bidi controls are removed by [`sanitize`].
pub fn sanitize_single_line(s: &str) -> String {
    let cleaned = sanitize(s);
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u{2028}"),
            '\u{2029}' => out.push_str("\\u{2029}"),
            _ => out.push(c),
        }
    }
    out
}

fn is_directional_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_sequences_whole_and_control_bytes() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m ok\u{7}"), "red ok");
        assert_eq!(sanitize("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(sanitize("\u{1b}]0;t\u{1b}\\after"), "after");
        assert_eq!(sanitize("\u{1b}cwiped"), "wiped");
        // Bare controls: C0, DEL, and C1 (a raw single-byte CSI) all drop.
        assert_eq!(sanitize("a\u{0}b\u{7f}c\u{9b}d"), "abcd");
        // A trailing lone ESC vanishes without panicking.
        assert_eq!(sanitize("tail\u{1b}"), "tail");
    }

    #[test]
    fn keeps_newlines_tabs_and_printable_unicode() {
        assert_eq!(sanitize("a\nb\tc"), "a\nb\tc");
        assert_eq!(sanitize("café → ok"), "café → ok");
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn single_line_neutralizes_line_column_and_bidi_spoofing() {
        assert_eq!(
            sanitize_single_line("file\nfake error\tkey\u{202e}txt\u{2028}tail"),
            "file\\nfake error\\tkeytxt\\u{2028}tail"
        );
    }
}
