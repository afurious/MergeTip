use mergetip_core::GithubInstallationId;
use mergetip_github::{GitHubApi, GitHubError, OctocrabGitHub, Reaction, RepoName};
use serde_json::{Value, json};
use time::OffsetDateTime;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path, query_param},
};

const INSTALLATION: GithubInstallationId = GithubInstallationId::new(1);

async fn setup() -> (MockServer, OctocrabGitHub) {
    let server = MockServer::start().await;
    let client = OctocrabGitHub::with_token("test-token", &server.uri()).unwrap();
    (server, client)
}

fn repo() -> RepoName {
    "acme/widgets".parse().unwrap()
}

fn review(id: i64) -> Value {
    json!({ "id": id, "state": "APPROVED", "user": { "id": 7, "login": "reviewer", "type": "User" } })
}

#[tokio::test]
async fn get_user_decodes_profile_fields() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/users/alice"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "login": "alice", "type": "User",
            "avatar_url": "https://avatars.example/42",
            "created_at": "2020-01-02T03:04:05Z",
            "email": null
        })))
        .mount(&server)
        .await;

    let user = client.get_user(INSTALLATION, "alice").await.unwrap();
    assert_eq!((user.id, user.login.as_str()), (42, "alice"));
    assert_eq!(
        user.created_at,
        Some(OffsetDateTime::from_unix_timestamp(1_577_934_245).unwrap())
    );
    assert!(!user.is_bot());
}

#[tokio::test]
async fn listings_follow_pages_until_a_short_page() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/42/reviews"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json((0..100).map(review).collect::<Vec<_>>()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/42/reviews"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json((100..103).map(review).collect::<Vec<_>>()),
        )
        .mount(&server)
        .await;

    let reviews = client
        .list_pull_reviews(INSTALLATION, &repo(), 42)
        .await
        .unwrap();
    assert_eq!(reviews.len(), 103);
    assert_eq!(reviews[102].id, 102);
}

#[tokio::test]
async fn listings_stop_at_the_page_limit() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                (0..100)
                    .map(|id| json!({ "id": id, "user": null }))
                    .collect::<Vec<_>>(),
            ),
        )
        .expect(10)
        .mount(&server)
        .await;

    let comments = client
        .list_issue_comments(INSTALLATION, &repo(), 7)
        .await
        .unwrap();
    assert_eq!(comments.len(), 1_000);
}

#[tokio::test]
async fn exhausted_rate_limit_reports_the_reset_time() {
    let (server, client) = setup().await;
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 120;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/1"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                .set_body_json(json!({ "message": "API rate limit exceeded" })),
        )
        .mount(&server)
        .await;

    let err = client.get_pull(INSTALLATION, &repo(), 1).await.unwrap_err();
    assert_eq!(
        err,
        GitHubError::RateLimited {
            reset_at: OffsetDateTime::from_unix_timestamp(reset).unwrap()
        }
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn secondary_rate_limit_uses_retry_after() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/1"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(json!({ "message": "You have exceeded a secondary rate limit" })),
        )
        .mount(&server)
        .await;

    let before = OffsetDateTime::now_utc();
    match client
        .get_issue(INSTALLATION, &repo(), 1)
        .await
        .unwrap_err()
    {
        GitHubError::RateLimited { reset_at } => {
            let wait = (reset_at - before).whole_seconds();
            assert!((29..=31).contains(&wait), "waits {wait}s");
        }
        other => panic!("expected a rate limit, got {other:?}"),
    }
}

#[tokio::test]
async fn plain_forbidden_and_missing_are_not_rate_limits() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/users/ghost"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/2"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "4999")
                .set_body_json(json!({ "message": "Resource not accessible by integration" })),
        )
        .mount(&server)
        .await;

    assert_eq!(
        client.get_user(INSTALLATION, "ghost").await.unwrap_err(),
        GitHubError::NotFound
    );
    let err = client
        .get_issue(INSTALLATION, &repo(), 2)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitHubError::Http { status: 403, .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn server_errors_are_retryable() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({ "message": "Bad Gateway" })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.get_pull(INSTALLATION, &repo(), 3).await.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn writes_send_the_expected_bodies() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/42/comments"))
        .and(body_json(json!({ "body": "⚡ hello" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 900, "html_url": "https://github.com/acme/widgets/pull/42#issuecomment-900"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/issues/comments/900"))
        .and(body_json(json!({ "body": "⚡ edited" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 900 })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/comments/555/reactions"))
        .and(body_json(json!({ "content": "rocket" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    let posted = client
        .create_comment(INSTALLATION, &repo(), 42, "⚡ hello")
        .await
        .unwrap();
    assert_eq!(posted.id, 900);
    client
        .update_comment(INSTALLATION, &repo(), 900, "⚡ edited")
        .await
        .unwrap();
    client
        .create_reaction(INSTALLATION, &repo(), 555, Reaction::Rocket)
        .await
        .unwrap();
}

#[tokio::test]
async fn logins_that_could_escape_the_path_are_refused() {
    let (_server, client) = setup().await;
    for login in ["../admin", "a/b", "a?x=1", ""] {
        assert!(matches!(
            client.get_user(INSTALLATION, login).await,
            Err(GitHubError::InvalidInput(_))
        ));
    }
}

#[test]
fn a_malformed_app_key_is_a_clear_error() {
    let err = OctocrabGitHub::from_app_key(1, b"not a pem", None).unwrap_err();
    assert!(
        matches!(err, GitHubError::InvalidInput(ref m) if m.contains("private key")),
        "{err:?}"
    );
}
