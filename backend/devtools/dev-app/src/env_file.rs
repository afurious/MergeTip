//! Updating `KEY=value` lines in a `.env` file without disturbing anything else in it.

/// Sets each `(key, value)` in `contents`: replaces the first `KEY=` line (commented-out lines
/// are left alone), or appends the pair at the end. Other lines, comments and order are kept.
pub fn upsert(contents: &str, updates: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    for (key, value) in updates {
        let line = format!("{key}={}", quote(value));
        let prefix = format!("{key}=");
        match lines
            .iter_mut()
            .find(|l| l.trim_start().starts_with(&prefix))
        {
            Some(existing) => *existing = line,
            None => lines.push(line),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Quotes values dotenv would otherwise misread (spaces, `#`, quotes).
fn quote(value: &str) -> String {
    if value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\'' | '\\'))
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_keys_and_appends_new_ones() {
        let before = "# GitHub\nGITHUB_APP_ID=\n# GITHUB_WEBHOOK_SECRET=old\nOTHER=keep\n";
        let after = upsert(
            before,
            &[
                ("GITHUB_APP_ID", "12345"),
                ("GITHUB_WEBHOOK_SECRET", "s3cret"),
            ],
        );
        assert_eq!(
            after,
            "# GitHub\nGITHUB_APP_ID=12345\n# GITHUB_WEBHOOK_SECRET=old\nOTHER=keep\nGITHUB_WEBHOOK_SECRET=s3cret\n"
        );
    }

    #[test]
    fn does_not_match_keys_that_share_a_prefix() {
        let after = upsert("GITHUB_APP_ID_OLD=1\n", &[("GITHUB_APP_ID", "2")]);
        assert_eq!(after, "GITHUB_APP_ID_OLD=1\nGITHUB_APP_ID=2\n");
    }

    #[test]
    fn quotes_values_dotenv_would_misread() {
        let after = upsert(
            "",
            &[
                ("A", "two words"),
                ("B", "x#y"),
                ("C", r#"say "hi""#),
                ("D", "plain"),
            ],
        );
        assert_eq!(
            after,
            "A=\"two words\"\nB=\"x#y\"\nC=\"say \\\"hi\\\"\"\nD=plain\n"
        );
    }

    #[test]
    fn works_on_an_empty_file() {
        assert_eq!(upsert("", &[("A", "1")]), "A=1\n");
    }
}
