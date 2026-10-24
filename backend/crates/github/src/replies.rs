//! Bot reply text (PRD "Bot reply templates"). Pure functions so every message is snapshot-tested.
//!
//! Anything a user typed (notes, logins from comment text) goes through [`escape`] or a code
//! span, so it cannot mention people or inject Markdown into a comment posted under the App's
//! name. A receiver is only @-mentioned once verification has shown they contributed to the
//! thread; before that, the login comes from the sender's text and mentioning it would let anyone
//! make the bot ping arbitrary users.

use mergetip_core::Sats;

/// Heading of the one collapsed status comment the bot keeps per thread.
pub const STATUS_HEADING: &str = "### ⚡ MergeTip";

/// Who and what a tip-related message is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipContext<'a> {
    pub sender: &'a str,
    pub receiver: &'a str,
    pub amount: Sats,
    pub note: Option<&'a str>,
}

pub fn receipt(tip: &TipContext<'_>, payment_hash: &str) -> String {
    let short_hash: String = payment_hash.chars().take(8).collect();
    format!(
        "⚡ @{} tipped @{} **{}**{} · payment `{short_hash}…`",
        tip.sender,
        tip.receiver,
        tip.amount,
        note_suffix(tip.note)
    )
}

pub fn held(tip: &TipContext<'_>) -> String {
    format!(
        "⏳ @{} will tip @{} **{}**{} when this PR is merged",
        tip.sender,
        tip.receiver,
        tip.amount,
        note_suffix(tip.note)
    )
}

/// Sent before verification, so the receiver is not mentioned.
pub fn connect_wallet(tip: &TipContext<'_>, link: &str) -> String {
    format!(
        "🔌 @{}, connect your wallet to send this **{}** tip to `{}` → [Connect wallet]({link}) \
         (held for 24 h)",
        tip.sender,
        tip.amount,
        code_span(tip.receiver)
    )
}

pub fn needs_confirmation(tip: &TipContext<'_>, threshold: Sats, link: &str) -> String {
    format!(
        "🔐 @{}, this **{}** tip to @{} is above your {} confirmation limit. Confirm within 1 \
         hour → [Confirm tip]({link})",
        tip.sender, tip.amount, tip.receiver, threshold
    )
}

/// Extra facts some rejection messages need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_tip_max: Sats,
    pub tips_per_hour: u32,
    pub tips_per_pair: u32,
}

/// The reply for a rejected tip, keyed by its `error_code`. Rejected receivers are never
/// mentioned: most rejections mean they haven't contributed, or the login isn't theirs to claim.
pub fn rejected(code: &str, tip: &TipContext<'_>, limits: &Limits, settings_link: &str) -> String {
    let sender = tip.sender;
    let receiver = code_span(tip.receiver);
    match code {
        "no_contribution" => format!(
            "😕 @{sender}, `{receiver}` hasn't contributed to this thread yet, so this tip can't \
             be verified."
        ),
        "unknown_user" => format!(
            "😕 @{sender}, there is no GitHub user `{receiver}`, so this tip can't be sent."
        ),
        "self_tip" => format!("😕 @{sender}, you can't tip yourself."),
        "bot_receiver" => {
            format!("😕 @{sender}, `{receiver}` is a bot account. Tips go to people.")
        }
        "account_too_new" => format!(
            "😕 @{sender}, `{receiver}`'s GitHub account is less than 30 days old, so it can't \
             receive tips yet."
        ),
        "on_merge_not_pr" => {
            format!("😕 @{sender}, `on-merge` only works on pull requests. Tip without it here.")
        }
        "rate_limited_sender" => format!(
            "😕 @{sender}, you've sent {} tips in the last hour. Try again a little later.",
            limits.tips_per_hour
        ),
        "pair_limit" => format!(
            "😕 @{sender}, you've already tipped `{receiver}` {} times in this thread.",
            limits.tips_per_pair
        ),
        "over_per_tip_cap" => format!(
            "😕 @{sender}, **{}** is above your per-tip limit of **{}**. [Manage limits]({settings_link})",
            tip.amount, limits.per_tip_max
        ),
        "verification_unavailable" => format!(
            "😕 @{sender}, I couldn't check this tip on GitHub (the thread may have moved or the \
             App lost access), so it wasn't sent."
        ),
        _ => format!("😕 @{sender}, this tip couldn't be sent."),
    }
}

/// One reply listing every malformed command in a comment, with the fix for each.
pub fn parse_errors(sender: &str, errors: &[(String, String)], syntax: &str) -> String {
    let mut out = format!("😕 @{sender}, I couldn't read ");
    out.push_str(if errors.len() == 1 {
        "this tip:\n"
    } else {
        "these tips:\n"
    });
    for (text, help) in errors {
        out.push_str(&format!("\n- `{}`: {help}", code_span(text)));
    }
    out.push_str(&format!("\n\nUsage: `{syntax}`"));
    out
}

/// Renders the collapsed status comment from its lines, oldest first.
pub fn status_comment(lines: &[String]) -> String {
    let mut out = format!("{STATUS_HEADING}\n");
    for line in lines {
        out.push_str("\n- ");
        out.push_str(line);
    }
    out
}

fn note_suffix(note: Option<&str>) -> String {
    note.map(|n| format!(" for {}", escape(n)))
        .unwrap_or_default()
}

/// Makes user text inert in Markdown: one line, no mentions, no formatting, links or HTML.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\r' | '\n' => out.push(' '),
            // A word joiner after `@` stops GitHub from turning it into a mention.
            '@' => out.push_str("@\u{2060}"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '!' | '#' | '|' | '~' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// Text placed inside a single-backtick code span: backticks would end the span early.
fn code_span(text: &str) -> String {
    text.replace('`', "'").replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        per_tip_max: Sats::new(10_000),
        tips_per_hour: 20,
        tips_per_pair: 3,
    };

    fn tip(note: Option<&str>) -> TipContext<'_> {
        TipContext {
            sender: "maintainer",
            receiver: "reviewer",
            amount: Sats::new(2_100),
            note,
        }
    }

    #[test]
    fn tip_messages() {
        let hash = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let rendered = [
            receipt(&tip(Some("the review")), hash),
            receipt(&tip(None), hash),
            held(&tip(None)),
            connect_wallet(&tip(None), "https://mergetip.dev/settings/wallet"),
            needs_confirmation(
                &TipContext {
                    amount: Sats::new(80_000),
                    ..tip(None)
                },
                Sats::new(50_000),
                "https://mergetip.dev/confirm/0b6f",
            ),
        ];
        insta::assert_snapshot!(rendered.join("\n\n"));
    }

    #[test]
    fn rejection_messages() {
        let codes = [
            "no_contribution",
            "unknown_user",
            "self_tip",
            "bot_receiver",
            "account_too_new",
            "on_merge_not_pr",
            "rate_limited_sender",
            "pair_limit",
            "over_per_tip_cap",
            "verification_unavailable",
            "something_new",
        ];
        let rendered: Vec<String> = codes
            .iter()
            .map(|code| {
                format!(
                    "{code}: {}",
                    rejected(
                        code,
                        &tip(None),
                        &LIMITS,
                        "https://mergetip.dev/settings/limits"
                    )
                )
            })
            .collect();
        insta::assert_snapshot!(rendered.join("\n"));
    }

    #[test]
    fn parse_error_reply_and_status_comment() {
        let errors = vec![
            (
                "/tip @alice two".to_owned(),
                "`two` isn't an amount.".to_owned(),
            ),
            ("/tip `x` 1".to_owned(), "Say who to tip.".to_owned()),
        ];
        insta::assert_snapshot!(parse_errors(
            "maintainer",
            &errors,
            "/tip @username <amount>"
        ));
        insta::assert_snapshot!(
            "status",
            status_comment(&[held(&tip(None)), receipt(&tip(None), "ffff0000ffff")])
        );
    }

    /// Only the authenticated sender may be mentioned before verification.
    #[test]
    fn unverified_receivers_are_never_mentioned() {
        let codes = [
            "no_contribution",
            "unknown_user",
            "self_tip",
            "bot_receiver",
            "account_too_new",
            "on_merge_not_pr",
            "rate_limited_sender",
            "pair_limit",
            "over_per_tip_cap",
            "verification_unavailable",
            "something_new",
        ];
        let target = TipContext {
            receiver: "torvalds",
            ..tip(None)
        };
        let mut messages: Vec<String> = codes
            .iter()
            .map(|code| rejected(code, &target, &LIMITS, "https://x"))
            .collect();
        messages.push(connect_wallet(&target, "https://x"));
        for message in messages {
            assert!(!message.contains("@torvalds"), "{message}");
        }
    }

    #[test]
    fn notes_cannot_mention_or_inject_markdown() {
        let escaped = escape("thanks @everyone ![x](http://evil) <b>hi</b>\nnext `code`");
        assert!(!escaped.contains("@everyone"));
        assert!(!escaped.contains('\n'));
        assert!(escaped.contains("\\!\\[x\\]"));
        assert!(escaped.contains("\\<b\\>"));
        assert!(escaped.contains("\\`code\\`"));
    }
}
