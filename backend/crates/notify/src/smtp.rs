//! Plain SMTP via lettre, for Mailpit in local development (`SMTP_URL=smtp://localhost:1025`).
//! It never uses TLS, so configuration refuses it outside dev; production sends through Resend.

use async_trait::async_trait;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{
        MultiPart,
        header::{HeaderName, HeaderValue},
    },
};

use crate::{Email, MailError, Mailer, SentEmail};

pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer").finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// `url` is `smtp://host[:port]` (port 25 by default).
    pub fn new(url: &str, from: &str) -> Result<Self, MailError> {
        let invalid = |what: &str| MailError::Rejected(format!("invalid SMTP_URL: {what}"));
        let rest = url
            .strip_prefix("smtp://")
            .ok_or_else(|| invalid("only smtp:// is supported (local Mailpit)"))?;
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| invalid("bad port"))?),
            None => (rest, 25),
        };
        if host.is_empty() {
            return Err(invalid("missing host"));
        }
        let from = from
            .parse()
            .map_err(|_| MailError::Rejected(format!("invalid EMAIL_FROM {from:?}")))?;
        Ok(Self {
            transport: AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
                .port(port)
                .build(),
            from,
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, email: &Email) -> Result<SentEmail, MailError> {
        let to = email
            .to
            .parse()
            .map_err(|_| MailError::Rejected("invalid recipient address".into()))?;
        let mut builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(&email.subject);
        for (name, value) in &email.headers {
            let name = HeaderName::new_from_ascii(name.clone())
                .map_err(|_| MailError::Rejected(format!("invalid header name {name}")))?;
            builder = builder.raw_header(HeaderValue::new(name, value.clone()));
        }
        let message = builder
            .multipart(MultiPart::alternative_plain_html(
                email.text.clone(),
                email.html.clone(),
            ))
            .map_err(|err| MailError::Rejected(err.to_string()))?;
        self.transport
            .send(message)
            .await
            .map_err(|err| MailError::Unavailable(err.to_string()))?;
        Ok(SentEmail { provider_id: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_smtp_urls_are_accepted() {
        assert!(SmtpMailer::new("smtp://localhost:1025", "MergeTip <tips@mergetip.dev>").is_ok());
        assert!(SmtpMailer::new("smtp://mailpit", "tips@mergetip.dev").is_ok());
        for bad in [
            "smtps://x:465",
            "localhost:1025",
            "smtp://",
            "smtp://x:port",
        ] {
            assert!(SmtpMailer::new(bad, "tips@mergetip.dev").is_err(), "{bad}");
        }
        assert!(SmtpMailer::new("smtp://x", "not an address").is_err());
    }
}

/// Sends one of each template to a real SMTP server, to eyeball them in Mailpit:
/// `docker compose up -d mailpit` then
/// `LIVE_SMTP_URL=smtp://localhost:1025 cargo test -p mergetip-notify -- --ignored live_smtp`,
/// and open <http://localhost:8025>.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::templates::{self, Notice, PledgeLine};

    #[tokio::test]
    #[ignore = "needs a running SMTP server; set LIVE_SMTP_URL"]
    async fn live_smtp_renders_every_template() {
        let url = std::env::var("LIVE_SMTP_URL").expect("LIVE_SMTP_URL");
        let mailer = SmtpMailer::new(&url, "MergeTip <tips@mail.mergetip.dev>").unwrap();
        let unsubscribe = "http://localhost:8080/v1/email/unsubscribe?token=example";
        let pledge = PledgeLine {
            sender: "maintainer".into(),
            amount: "5,000 sats".into(),
            note: Some("the review".into()),
            thread: "acme/widgets#42".into(),
            thread_url: "https://github.com/acme/widgets/pull/42".into(),
            claim_url: Some("http://localhost:5173/claim/example".into()),
            expires_on: "2026-11-25".into(),
        };
        let expired = PledgeLine {
            claim_url: None,
            ..pledge.clone()
        };
        let rendered = [
            ("pledge", templates::pledge(&pledge, unsubscribe)),
            ("reminder", templates::reminder(&pledge, unsubscribe)),
            ("expiry", templates::expiry(&expired, unsubscribe)),
            (
                "digest",
                templates::digest(
                    &[
                        (Notice::New, pledge.clone()),
                        (Notice::Reminder, pledge.clone()),
                        (Notice::Expired, expired.clone()),
                    ],
                    unsubscribe,
                ),
            ),
            (
                "otp",
                templates::otp("042137", "80,000 sats", "reviewer", unsubscribe),
            ),
        ];
        for (tag, rendered) in rendered {
            let email = crate::email(
                "reviewer@example.com".into(),
                rendered,
                unsubscribe,
                tag,
                None,
            );
            mailer.send(&email).await.unwrap();
        }
    }
}
