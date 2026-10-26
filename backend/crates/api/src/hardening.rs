//! Abuse protection and response headers (PRD "Security, privacy and abuse prevention").
//!
//! Rate limits use `governor` (GCRA, the engine behind `tower_governor`) directly, so a refusal
//! is our JSON `429 rate_limited` with `Retry-After` rather than a plain-text body. Each guarded
//! route group has a per-IP limit and, when the request carries a session or extension token, a
//! per-user limit too, so one account can't fan out across addresses.

use std::{net::SocketAddr, num::NonZeroU32, time::Duration};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use governor::{
    DefaultKeyedRateLimiter, Quota, RateLimiter,
    clock::{Clock, DefaultClock},
};

use crate::{ApiError, AppState, cookies};

/// Keys kept per limiter before stale ones are dropped (bounded memory under a flood).
const MAX_TRACKED_KEYS: usize = 50_000;

struct Rule {
    per_ip: DefaultKeyedRateLimiter<String>,
    per_user: DefaultKeyedRateLimiter<String>,
}

impl Rule {
    fn new(ip_per_minute: u32, user_per_minute: u32) -> Self {
        let quota = |n: u32| Quota::per_minute(NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN));
        Self {
            per_ip: RateLimiter::keyed(quota(ip_per_minute)),
            per_user: RateLimiter::keyed(quota(user_per_minute)),
        }
    }
}

/// The limits for the routes worth abusing: sign-in, claim links, confirmation codes and
/// comments posted on a user's behalf.
pub struct RateLimits {
    auth: Rule,
    claims: Rule,
    otp: Rule,
    comment: Rule,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            auth: Rule::new(30, 30),
            claims: Rule::new(20, 10),
            otp: Rule::new(5, 5),
            comment: Rule::new(60, 30),
        }
    }
}

impl RateLimits {
    fn rule_for(&self, path: &str) -> Option<(&'static str, &Rule)> {
        if path.starts_with("/v1/auth/") {
            Some(("auth", &self.auth))
        } else if path.starts_with("/v1/claims/") {
            Some(("claims", &self.claims))
        } else if path.starts_with("/v1/confirmations/") && path.ends_with("/otp") {
            Some(("otp", &self.otp))
        } else if path == "/v1/tips/comment" {
            Some(("comment", &self.comment))
        } else {
            None
        }
    }
}

/// The client's address: Fly's `Fly-Client-IP` (set by its proxy, which overwrites any value the
/// client sent), else the TCP peer.
fn client_ip(request: &Request) -> String {
    request
        .headers()
        .get("fly-client-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

/// The caller's credential, hashed (never kept raw), when there is one.
fn user_key(request: &Request) -> Option<String> {
    let headers = request.headers();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| cookies::get(headers, cookies::SESSION))?;
    Some(mergetip_crypto::hash_token(token.trim()))
}

fn too_many(wait: Duration) -> Response {
    let mut response = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many requests. Wait a moment and try again.",
    )
    .into_response();
    let seconds = wait.as_secs().max(1).to_string();
    if let Ok(value) = HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub async fn rate_limit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let limits = state.rate_limits();
    let Some((group, rule)) = limits.rule_for(request.uri().path()) else {
        return next.run(request).await;
    };
    let clock = DefaultClock::default();
    let keys = [
        Some((&rule.per_ip, format!("{group}:{}", client_ip(&request)))),
        user_key(&request).map(|user| (&rule.per_user, format!("{group}:{user}"))),
    ];
    for (limiter, key) in keys.into_iter().flatten() {
        if limiter.len() > MAX_TRACKED_KEYS {
            limiter.retain_recent();
        }
        if let Err(not_until) = limiter.check_key(&key) {
            tracing::info!(group, "rate limited");
            return too_many(not_until.wait_time_from(clock.now()));
        }
    }
    next.run(request).await
}

/// Headers on every response. Credentialed responses are also `no-store`, so a shared cache or
/// the browser's back button never replays a user's data.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let credentialed = request.headers().contains_key(header::AUTHORIZATION)
        || cookies::get(request.headers(), cookies::SESSION).is_some();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=63072000; includeSubDomains"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if credentialed {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
