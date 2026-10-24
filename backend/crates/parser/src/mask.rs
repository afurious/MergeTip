//! Hides the parts of a GitHub comment where a `/tip` must not count: fenced code blocks, inline
//! code, `>` quoted lines and HTML comments.
//!
//! Masked text is replaced with spaces byte for byte (a multi-byte character becomes as many
//! spaces as it has bytes), so the masked body has the same lines and byte offsets as the
//! original: a `/tip` found in the masked text can be read from the original at the same offset.

/// Returns `body` with the ignored regions blanked out, byte-aligned with `body`.
pub(crate) fn mask(body: &str) -> String {
    let without_comments = blank_html_comments(body);
    let mut out = String::with_capacity(without_comments.len());
    let mut fence: Option<(char, usize)> = None;

    for (i, line) in without_comments.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if closes_fence(trimmed, open) {
                fence = None;
            }
            out.push_str(&blank(line));
        } else if let Some(open) = opens_fence(trimmed) {
            fence = Some(open);
            out.push_str(&blank(line));
        } else if trimmed.starts_with('>') {
            out.push_str(&blank(line));
        } else {
            out.push_str(&blank_inline_code(line));
        }
    }
    out
}

/// A fence is three or more backticks or tildes at the start of a line (after indentation).
fn opens_fence(trimmed: &str) -> Option<(char, usize)> {
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = trimmed.chars().take_while(|c| *c == marker).count();
    (len >= 3).then_some((marker, len))
}

/// A closing fence uses the same character, at least as many times, and nothing else.
fn closes_fence(trimmed: &str, (marker, len): (char, usize)) -> bool {
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    run >= len && trimmed[run * marker.len_utf8()..].trim().is_empty()
}

fn blank(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' || c == '\r' {
            out.push(c);
        } else {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
        }
    }
    out
}

fn blank_html_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => {
                let comment_end = start + 4 + end + 3;
                out.push_str(&blank(&rest[start..comment_end]));
                rest = &rest[comment_end..];
            }
            None => {
                // Unterminated: GitHub hides everything after it, so we do too.
                out.push_str(&blank(&rest[start..]));
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Blanks backtick code spans: a run of N backticks up to the next run of exactly N. An
/// unmatched run is literal text, as in CommonMark. Works on bytes: backticks are ASCII, so every
/// span boundary is a character boundary.
fn blank_inline_code(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open_len = run_len(bytes, i);
        let mut j = i + open_len;
        let mut closed = None;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let len = run_len(bytes, j);
                if len == open_len {
                    closed = Some(j + len);
                    break;
                }
                j += len;
            } else {
                j += 1;
            }
        }
        match closed {
            Some(end) => {
                out.push_str(&line[copied..i]);
                out.push_str(&blank(&line[i..end]));
                copied = end;
                i = end;
            }
            None => i += open_len,
        }
    }
    out.push_str(&line[copied..]);
    out
}

fn run_len(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().take_while(|b| **b == b'`').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_line_count() {
        let body = "a\n```\n/tip @x 1\n```\n> q\nb";
        assert_eq!(mask(body).lines().count(), body.lines().count());
    }

    #[test]
    fn blanks_fenced_blocks_including_tilde_fences() {
        let masked = mask("```rust\n/tip @a 1\n```\n~~~\n/tip @b 2\n~~~\n/tip @c 3");
        assert!(!masked.contains("@a") && !masked.contains("@b"));
        assert!(masked.contains("/tip @c 3"));
    }

    #[test]
    fn a_shorter_fence_does_not_close_a_longer_one() {
        let masked = mask("````\n```\n/tip @a 1\n````\n/tip @b 2");
        assert!(!masked.contains("@a"));
        assert!(masked.contains("@b"));
    }

    #[test]
    fn blanks_quotes_inline_code_and_comments() {
        let masked =
            mask("> /tip @a 1\nsee `/tip @b 2` and ``/tip `@c` 3``\n<!-- /tip @d 4\n-->/tip @e 5");
        for hidden in ["@a", "@b", "@c", "@d"] {
            assert!(!masked.contains(hidden), "{hidden} visible in {masked:?}");
        }
        assert!(masked.contains("/tip @e 5"));
    }

    #[test]
    fn masked_text_is_byte_aligned_with_the_original() {
        let body = "héllo `cödé` ✨\n> ünïcode quote\n/tip @a 1 for café";
        let masked = mask(body);
        assert_eq!(masked.len(), body.len());
        for (m, o) in masked.split('\n').zip(body.split('\n')) {
            assert_eq!(m.len(), o.len());
        }
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        assert_eq!(mask("it`s /tip @a 1"), "it`s /tip @a 1");
    }
}
