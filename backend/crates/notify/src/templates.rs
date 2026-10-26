//! Email bodies (askama, checked at compile time). Every message has an HTML and a plain-text
//! part; the HTML templates escape everything they interpolate, so a tip note can't inject markup.

use askama::Template;

/// A pledge as an email shows it. Values are display-ready (`5,000 sats`, `2026-11-25`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PledgeLine {
    pub sender: String,
    pub amount: String,
    pub note: Option<String>,
    /// `owner/repo#number`.
    pub thread: String,
    pub thread_url: String,
    /// `None` once the pledge can no longer be claimed (expired).
    pub claim_url: Option<String>,
    pub expires_on: String,
}

/// Why a pledge is in a digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    New,
    Reminder,
    Expired,
    /// Still open with nothing new to say; listed so a digest shows everything waiting.
    Open,
}

impl Notice {
    fn status(self, pledge: &PledgeLine) -> String {
        match self {
            Self::New => format!("new, claim by {}", pledge.expires_on),
            Self::Reminder => format!("expires on {}", pledge.expires_on),
            Self::Expired => "expired unclaimed; nothing was charged".into(),
            Self::Open => format!("claim by {}", pledge.expires_on),
        }
    }
}

/// A rendered message, before addressing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub html: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "pledge.html")]
struct PledgeHtml<'a> {
    subject: &'a str,
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "pledge.txt")]
struct PledgeText<'a> {
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "reminder.html")]
struct ReminderHtml<'a> {
    subject: &'a str,
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "reminder.txt")]
struct ReminderText<'a> {
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "expiry.html")]
struct ExpiryHtml<'a> {
    subject: &'a str,
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "expiry.txt")]
struct ExpiryText<'a> {
    pledge: &'a PledgeLine,
    unsubscribe_url: &'a str,
}

struct DigestItem<'a> {
    pledge: &'a PledgeLine,
    status: String,
}

#[derive(Template)]
#[template(path = "digest.html")]
struct DigestHtml<'a> {
    subject: &'a str,
    open: usize,
    items: &'a [DigestItem<'a>],
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "digest.txt")]
struct DigestText<'a> {
    open: usize,
    items: &'a [DigestItem<'a>],
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "otp.html")]
struct OtpHtml<'a> {
    subject: &'a str,
    code: &'a str,
    amount: &'a str,
    receiver: &'a str,
    unsubscribe_url: &'a str,
}

#[derive(Template)]
#[template(path = "otp.txt")]
struct OtpText<'a> {
    code: &'a str,
    amount: &'a str,
    receiver: &'a str,
    unsubscribe_url: &'a str,
}

fn render(template: &impl Template) -> String {
    // Templates are compiled in and take only owned strings, so rendering can only fail on a
    // formatter error, which `String` never returns.
    template
        .render()
        .expect("rendering into a String cannot fail")
}

/// A new pledge, the first notice.
pub fn pledge(pledge: &PledgeLine, unsubscribe_url: &str) -> Rendered {
    let subject = format!("@{} sent you {} on GitHub", pledge.sender, pledge.amount);
    Rendered {
        html: render(&PledgeHtml {
            subject: &subject,
            pledge,
            unsubscribe_url,
        }),
        text: render(&PledgeText {
            pledge,
            unsubscribe_url,
        }),
        subject,
    }
}

/// Seven days left (day 23).
pub fn reminder(pledge: &PledgeLine, unsubscribe_url: &str) -> Rendered {
    let subject = format!(
        "Your {} from @{} expires in 7 days",
        pledge.amount, pledge.sender
    );
    Rendered {
        html: render(&ReminderHtml {
            subject: &subject,
            pledge,
            unsubscribe_url,
        }),
        text: render(&ReminderText {
            pledge,
            unsubscribe_url,
        }),
        subject,
    }
}

/// The final notice: the pledge expired unclaimed.
pub fn expiry(pledge: &PledgeLine, unsubscribe_url: &str) -> Rendered {
    let subject = format!(
        "The {} from @{} expired unclaimed",
        pledge.amount, pledge.sender
    );
    Rendered {
        html: render(&ExpiryHtml {
            subject: &subject,
            pledge,
            unsubscribe_url,
        }),
        text: render(&ExpiryText {
            pledge,
            unsubscribe_url,
        }),
        subject,
    }
}

/// Several notices in one email (at most one email per receiver per day).
pub fn digest(items: &[(Notice, PledgeLine)], unsubscribe_url: &str) -> Rendered {
    let rows: Vec<DigestItem<'_>> = items
        .iter()
        .map(|(notice, pledge)| DigestItem {
            pledge,
            status: notice.status(pledge),
        })
        .collect();
    let open = items.iter().filter(|(n, _)| *n != Notice::Expired).count();
    let subject = if open == 0 {
        "Your MergeTip tips expired unclaimed".to_owned()
    } else {
        format!(
            "You have {open} tip{} waiting on MergeTip",
            if open == 1 { "" } else { "s" }
        )
    };
    Rendered {
        html: render(&DigestHtml {
            subject: &subject,
            open,
            items: &rows,
            unsubscribe_url,
        }),
        text: render(&DigestText {
            open,
            items: &rows,
            unsubscribe_url,
        }),
        subject,
    }
}

/// A confirmation code for a high-value tip.
pub fn otp(code: &str, amount: &str, receiver: &str, unsubscribe_url: &str) -> Rendered {
    // The code stays out of the subject: subjects show in lock-screen previews and logs.
    let subject = "Your MergeTip confirmation code".to_owned();
    Rendered {
        html: render(&OtpHtml {
            subject: &subject,
            code,
            amount,
            receiver,
            unsubscribe_url,
        }),
        text: render(&OtpText {
            code,
            amount,
            receiver,
            unsubscribe_url,
        }),
        subject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNSUBSCRIBE: &str = "https://api.mergetip.dev/v1/email/unsubscribe?token=u.abc";

    fn line(note: Option<&str>) -> PledgeLine {
        PledgeLine {
            sender: "maintainer".into(),
            amount: "5,000 sats".into(),
            note: note.map(str::to_owned),
            thread: "acme/widgets#42".into(),
            thread_url: "https://github.com/acme/widgets/pull/42".into(),
            claim_url: Some("https://mergetip.dev/claim/q2V3".into()),
            expires_on: "2026-11-25".into(),
        }
    }

    fn both(rendered: &Rendered) -> String {
        format!(
            "Subject: {}\n\n=== text ===\n{}\n=== html ===\n{}",
            rendered.subject, rendered.text, rendered.html
        )
    }

    #[test]
    fn pledge_email() {
        insta::assert_snapshot!(both(&pledge(&line(Some("the review")), UNSUBSCRIBE)));
    }

    #[test]
    fn reminder_email() {
        insta::assert_snapshot!(both(&reminder(&line(None), UNSUBSCRIBE)));
    }

    #[test]
    fn expiry_email() {
        let expired = PledgeLine {
            claim_url: None,
            ..line(None)
        };
        insta::assert_snapshot!(both(&expiry(&expired, UNSUBSCRIBE)));
    }

    #[test]
    fn digest_email() {
        let items = [
            (Notice::New, line(Some("the review"))),
            (
                Notice::Reminder,
                PledgeLine {
                    sender: "octo".into(),
                    amount: "21 sats".into(),
                    thread: "acme/gadgets#7".into(),
                    thread_url: "https://github.com/acme/gadgets/issues/7".into(),
                    expires_on: "2026-11-02".into(),
                    ..line(None)
                },
            ),
            (
                Notice::Expired,
                PledgeLine {
                    claim_url: None,
                    ..line(None)
                },
            ),
        ];
        insta::assert_snapshot!(both(&digest(&items, UNSUBSCRIBE)));
    }

    #[test]
    fn otp_email() {
        insta::assert_snapshot!(both(&otp("042137", "80,000 sats", "reviewer", UNSUBSCRIBE)));
    }

    #[test]
    fn notes_cannot_inject_html() {
        let rendered = pledge(
            &line(Some("<script>alert(1)</script> & <b>bold</b>")),
            UNSUBSCRIBE,
        );
        assert!(!rendered.html.contains("<script>"));
        assert!(
            rendered.html.contains("&#60;script&#62;") || rendered.html.contains("&lt;script&gt;")
        );
    }
}
