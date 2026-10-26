//! `POST /v1/tips/comment`: the extension's ⚡ Tip button (FR-8.4). The API only posts a `/tip`
//! comment as the user; the signed webhook for that comment then creates the tip like any
//! other, so there is no second way to pay.

use axum::{Json, extract::State, http::StatusCode};
use mergetip_github::{RepoName, oauth::OAuthError};
use mergetip_parser::parse_comment;
use mergetip_store::installations;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ApiError, AppState, extract::AuthUser, github_token, routes::tips::TipKindBody};

/// The largest tip the grammar accepts (PRD).
const MAX_SATS: u64 = 100_000_000;
const MAX_NOTE_CHARS: usize = 140;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TipCommentRequest {
    /// `owner/name`.
    repo: String,
    thread_number: u32,
    /// GitHub login, without `@`.
    receiver: String,
    amount_sats: u64,
    #[schema(inline)]
    kind: TipKindBody,
    note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PostedTipComment {
    pub comment_url: String,
}

fn invalid(message: &'static str) -> ApiError {
    ApiError::unprocessable("invalid_tip", message)
}

/// The note as it may appear in a comment: one line, no backticks, no nested `/tip`.
fn clean_note(note: &str) -> String {
    let one_line: String = note
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .filter(|c| *c != '`')
        .collect();
    let mut cleaned = one_line;
    while let Some(at) = cleaned.to_ascii_lowercase().find("/tip") {
        cleaned.replace_range(at..at + 4, "");
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `/tip` line for a request, checked by the same parser the webhook uses: it must read
/// back as exactly the tip that was asked for.
fn command(body: &TipCommentRequest) -> Result<String, ApiError> {
    if body.amount_sats == 0 || body.amount_sats > MAX_SATS {
        return Err(invalid(
            "The amount must be between 1 and 100,000,000 sats.",
        ));
    }
    let note = body
        .note
        .as_deref()
        .map(clean_note)
        .filter(|n| !n.is_empty());
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
    {
        return Err(invalid("The note can be at most 140 characters."));
    }
    let on_merge = body.kind == TipKindBody::OnMerge;
    let mut line = format!("/tip @{} {}", body.receiver, body.amount_sats);
    if on_merge {
        line.push_str(" on-merge");
    }
    if let Some(note) = &note {
        line.push_str(" for ");
        line.push_str(note);
    }
    match parse_comment(&line).as_slice() {
        [Ok(parsed)]
            if parsed.receiver.eq_ignore_ascii_case(&body.receiver)
                && parsed.amount.get() == body.amount_sats
                && parsed.on_merge == on_merge
                && parsed.note == note =>
        {
            Ok(line)
        }
        _ => Err(invalid("That isn't a valid GitHub username.")),
    }
}

/// Post a `/tip` comment on GitHub as the signed-in user.
#[utoipa::path(
    post,
    path = "/v1/tips/comment",
    operation_id = "post_tip_comment",
    tag = "tips",
    request_body = TipCommentRequest,
    responses(
        (status = 202, body = PostedTipComment, description = "Posted; the webhook takes it from here"),
        (status = 401, description = "`unauthenticated` or `github_reauth_required`", body = crate::error::ErrorBody),
        (status = 403, description = "`app_not_installed` or `github_forbidden`", body = crate::error::ErrorBody),
        (status = 422, description = "`invalid_tip`", body = crate::error::ErrorBody),
    ),
    security(("session" = []), ("bearer" = []))
)]
pub async fn post_tip_comment(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<TipCommentRequest>,
) -> Result<(StatusCode, Json<PostedTipComment>), ApiError> {
    let repo: RepoName = body
        .repo
        .parse()
        .map_err(|_| invalid("The repository must be owner/name."))?;
    if body.thread_number == 0 {
        return Err(invalid("The thread number must be positive."));
    }
    let line = command(&body)?;
    if installations::find_for_repo(state.pool(), &repo.to_string())
        .await?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "app_not_installed",
            "The MergeTip app isn't installed on this repository.",
        ));
    }

    let token = github_token::for_user(&state, auth.user.id).await?;
    let posted = state
        .oauth()
        .create_comment(token.expose(), &repo, body.thread_number, &line)
        .await
        .map_err(|err| match err {
            OAuthError::Forbidden(_) => ApiError::new(
                StatusCode::FORBIDDEN,
                "github_forbidden",
                "GitHub didn't let you comment on that thread.",
            ),
            OAuthError::Rejected(_) => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "github_reauth_required",
                "Sign in with GitHub again to continue.",
            ),
            other => {
                tracing::warn!(error = %other, "posting a tip comment failed");
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "github_unavailable",
                    "GitHub didn't answer. Try again in a moment.",
                )
            }
        })?;
    tracing::info!(user_id = %auth.user.id, repo = %repo, "tip comment posted for the user");
    Ok((
        StatusCode::ACCEPTED,
        Json(PostedTipComment {
            comment_url: posted.html_url,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_flattened_and_defused() {
        assert_eq!(clean_note("thanks\nfor `this`"), "thanks for this");
        assert_eq!(clean_note("/tip @evil 1000000"), "@evil 1000000");
        assert_eq!(clean_note("a /TIP b /tip/tip c"), "a b c");
    }
}
