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
        } else if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
        // Any other control char (C0, DEL, C1) is dropped.
    }
    out
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
}
