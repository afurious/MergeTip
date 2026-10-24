//! Hides the parts of a GitHub comment where a `/tip` must not count: fenced and indented code
//! blocks, inline code, `>` quoted lines and HTML comments.
//!
//! Masked text is replaced with spaces byte for byte (a multi-byte character becomes as many
//! spaces as it has bytes), so the masked body has the same lines and byte offsets as the
//! original: a `/tip` found in the masked text can be read from the original at the same offset.

/// Returns `body` with the ignored regions blanked out, byte-aligned with `body`.
pub(crate) fn mask(body: &str) -> String {
    let without_comments = blank_html_comments(body);
    let mut out = String::with_capacity(without_comments.len());
    let mut fence: Option<(char, usize)> = None;
    // CommonMark: an indented code block can't interrupt a paragraph, so it needs a blank line
    // (or the start of the comment, or a fenced block) before it. Inside a list, an indented
    // line after a blank one continues the list item instead.
    let mut after_break = true;
    let mut in_indented_code = false;
    let mut in_list = false;

    for (i, line) in without_comments.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if closes_fence(trimmed, open) {
                fence = None;
                after_break = true;
            }
            out.push_str(&blank(line));
            continue;
        }
        if trimmed.trim().is_empty() {
            after_break = true;
            out.push_str(line);
            continue;
        }
        let indent = indent_width(line);
        if indent >= 4 && (in_indented_code || (after_break && !in_list)) {
            in_indented_code = true;
            after_break = false;
            out.push_str(&blank(line));
            continue;
        }
        in_indented_code = false;
        after_break = false;

        if let Some(open) = opens_fence(trimmed) {
            fence = Some(open);
            out.push_str(&blank(line));
        } else if trimmed.starts_with('>') {
            out.push_str(&blank(line));
        } else {
            if indent < 4 && is_list_item(trimmed) {
                in_list = true;
            } else if indent == 0 {
                in_list = false;
            }
            out.push_str(&blank_inline_code(line));
        }
    }
    out
}

/// Leading indentation in columns, with tabs advancing to the next multiple of 4.
fn indent_width(line: &str) -> usize {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += 4 - width % 4,
            _ => break,
        }
    }
    width
}

/// `- `, `* `, `+ `, `1. ` or `1) ` list markers.
fn is_list_item(trimmed: &str) -> bool {
    let bullet = ["- ", "* ", "+ "].iter().any(|m| trimmed.starts_with(m))
        || matches!(trimmed, "-" | "*" | "+");
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let ordered = (1..=9).contains(&digits)
        && trimmed[digits..]
            .strip_prefix(['.', ')'])
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '));
    bullet || ordered
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
    fn indented_code_blocks_are_masked() {
        let masked =
            mask("Usage:\n\n    /tip @a 1\n\n    /tip @b 2\n\t/tip @c 3\nback to text /tip @d 4");
        for hidden in ["@a", "@b", "@c"] {
            assert!(!masked.contains(hidden), "{hidden} visible in {masked:?}");
        }
        assert!(masked.contains("/tip @d 4"));
        assert!(
            !mask("    /tip @first 1").contains("@first"),
            "start of comment counts"
        );
    }

    #[test]
    fn indentation_that_is_not_a_code_block_is_kept() {
        // Continues the paragraph above (no blank line), so GitHub renders it as text.
        assert!(mask("Thanks!\n    /tip @a 1").contains("/tip @a 1"));
        // A list item's continuation paragraph.
        assert!(mask("- great review\n\n    /tip @b 2").contains("/tip @b 2"));
        assert!(mask("1. step one\n\n    /tip @c 3").contains("/tip @c 3"));
        // Three spaces is not enough for a code block.
        assert!(mask("\n   /tip @d 4").contains("/tip @d 4"));
    }

    #[test]
    fn a_list_ends_at_an_unindented_paragraph() {
        let masked = mask("- item\n\nParagraph.\n\n    /tip @a 1");
        assert!(!masked.contains("@a"));
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        assert_eq!(mask("it`s /tip @a 1"), "it`s /tip @a 1");
    }
}
