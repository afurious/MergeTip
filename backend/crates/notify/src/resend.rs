//! [Resend](https://resend.com/docs/api-reference/emails/send-email): the production provider.

use std::time::Duration;

use async_trait::async_trait;
use mergetip_crypto::Redacted;
use serde::Deserialize;
use serde_json::json;

use crate::{Email, MailError, Mailer, SentEmail};

pub const DEFAULT_BASE_URL: &str = "https://api.resend.com";

pub struct ResendMailer {
    http: reqwest::Client,
    base_url: String,
    api_key: Redacted<String>,
    from: String,
}

impl std::fmt::Debug for ResendMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResendMailer")
            .field("base_url", &self.base_url)
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

impl ResendMailer {
    pub fn new(base_url: &str, api_key: Redacted<String>, from: String) -> Result<Self, MailError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mergetip/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| MailError::Unavailable(err.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            from,
        })
    }
}

#[derive(Deserialize)]
struct Accepted {
    id: String,
}

#[async_trait]
impl Mailer for ResendMailer {
    async fn send(&self, email: &Email) -> Result<SentEmail, MailError> {
        let headers: serde_json::Map<String, serde_json::Value> = email
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), json!(value)))
            .collect();
        let body = json!({
            "from": self.from,
            "to": [email.to],
            "subject": email.subject,
            "html": email.html,
            "text": email.text,
            "headers": headers,
            "tags": [{ "name": "kind", "value": email.tag }],
        });
        let mut request = self
            .http
            .post(format!("{}/emails", self.base_url))
            .bearer_auth(self.api_key.expose())
            .json(&body);
        if let Some(key) = &email.idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request
            .send()
            .await
            .map_err(|err| MailError::Unavailable(err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            let accepted: Accepted = response
                .json()
                .await
                .map_err(|err| MailError::Unavailable(format!("unreadable response: {err}")))?;
            return Ok(SentEmail {
                provider_id: Some(accepted.id),
            });
        }
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Err(MailError::Unavailable(format!("{status}: {detail}")))
        } else {
            Err(MailError::Rejected(format!("{status}: {detail}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header, method, path},
    };

    use super::*;

    fn email() -> Email {
        Email {
            to: "alice@example.com".into(),
            subject: "Hi".into(),
            html: "<p>Hi</p>".into(),
            text: "Hi".into(),
            headers: vec![("List-Unsubscribe".into(), "<https://x/u>".into())],
            tag: "pledge".into(),
            idempotency_key: Some("digest:u:2026-11-02".into()),
        }
    }

    async fn mailer(server: &MockServer) -> ResendMailer {
        ResendMailer::new(
            &server.uri(),
            Redacted::new("re_test".into()),
            "MergeTip <tips@mergetip.dev>".into(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sends_with_auth_headers_and_idempotency_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .and(header("authorization", "Bearer re_test"))
            .and(header("idempotency-key", "digest:u:2026-11-02"))
            .and(body_partial_json(json!({
                "from": "MergeTip <tips@mergetip.dev>",
                "to": ["alice@example.com"],
                "headers": { "List-Unsubscribe": "<https://x/u>" },
                "tags": [{ "name": "kind", "value": "pledge" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "em_1" })))
            .expect(1)
            .mount(&server)
            .await;
        let sent = mailer(&server).await.send(&email()).await.unwrap();
        assert_eq!(sent.provider_id.as_deref(), Some("em_1"));
    }

    #[tokio::test]
    async fn outages_retry_and_refusals_do_not() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_string("invalid `to`"))
            .mount(&server)
            .await;
        let mailer = mailer(&server).await;
        assert!(mailer.send(&email()).await.unwrap_err().is_retryable());
        let refused = mailer.send(&email()).await.unwrap_err();
        assert!(matches!(refused, MailError::Rejected(_)), "{refused}");
    }
}
