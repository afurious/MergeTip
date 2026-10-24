//! Minimal cookie handling: the API sets two cookies (`mt_session`, `mt_oauth`) and reads them
//! back; a full cookie jar dependency isn't warranted.

use axum::http::{HeaderMap, HeaderValue, header};

pub const SESSION: &str = "mt_session";
pub const OAUTH_STATE: &str = "mt_oauth";

/// The value of cookie `name` from the request's `Cookie` headers.
pub fn get<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// A `Set-Cookie` value. Always `HttpOnly` and `SameSite=Lax`: scripts never need these cookies,
/// and Lax still sends them on the top-level redirect back from GitHub.
pub fn set(name: &str, value: &str, path: &str, max_age_secs: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie names and values are token characters")
}

/// A `Set-Cookie` value that deletes the cookie.
pub fn clear(name: &str, path: &str, secure: bool) -> HeaderValue {
    set(name, "", path, 0, secure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_cookie_among_several_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; mt_session=abc"),
        );
        headers.append(header::COOKIE, HeaderValue::from_static("mt_oauth=xyz"));
        assert_eq!(get(&headers, SESSION), Some("abc"));
        assert_eq!(get(&headers, OAUTH_STATE), Some("xyz"));
        assert_eq!(get(&headers, "missing"), None);
    }

    #[test]
    fn set_cookie_has_safe_attributes() {
        let value = set(SESSION, "tok", "/", 60, true);
        assert_eq!(
            value,
            "mt_session=tok; Path=/; Max-Age=60; HttpOnly; SameSite=Lax; Secure"
        );
        assert!(
            !set(SESSION, "tok", "/", 60, false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
        assert!(
            clear(SESSION, "/", true)
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
    }
}
