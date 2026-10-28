//! Browser-bound OIDC authorization and session cookies.
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use thalamus_identity::{Error as IdentityError, LoginStart};

use crate::{ApiError, AppState};

type HmacSha256 = Hmac<Sha256>;

const LOGIN_COOKIE: &str = "__Host-thalamus-oidc";
const SESSION_COOKIE: &str = "__Host-thalamus-session";
const LOGIN_LIFETIME_SECONDS: u64 = 600;
const LOGIN_COOKIE_DOMAIN: &[u8] = b"thalamus:oidc-login-cookie:v1:";
const CLEAR_LOGIN_COOKIE: &str =
    "__Host-thalamus-oidc=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax";
const CLEAR_SESSION_COOKIE: &str =
    "__Host-thalamus-session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
}

fn unauthorized() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "invalid or expired login state")
}

fn internal_error() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "authentication configuration error",
    )
}

fn now_seconds() -> Result<u64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| internal_error())
}

fn state_mac(key: &[u8], payload: &str) -> Result<HmacSha256, ApiError> {
    // The same key can sign identity sessions. Domain separation prevents a login
    // cookie from being interpreted as a session token, or vice versa.
    if key.len() < 32 {
        return Err(internal_error());
    }
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| internal_error())?;
    mac.update(LOGIN_COOKIE_DOMAIN);
    mac.update(payload.as_bytes());
    Ok(mac)
}

fn sign_login_state(key: &[u8], state: &str, now: u64) -> Result<String, ApiError> {
    if state.is_empty() {
        return Err(internal_error());
    }
    let expires = now
        .checked_add(LOGIN_LIFETIME_SECONDS)
        .ok_or_else(internal_error)?;
    let payload = format!("v1.{expires}.{}", URL_SAFE_NO_PAD.encode(state.as_bytes()));
    let tag = URL_SAFE_NO_PAD.encode(state_mac(key, &payload)?.finalize().into_bytes());
    let token = format!("{payload}.{tag}");
    // Keep the entire Set-Cookie value within the usual browser cookie size limit.
    if token.len() + 100 > 4096 {
        return Err(internal_error());
    }
    Ok(token)
}

fn verify_login_state(key: &[u8], token: &str, now: u64) -> Result<String, ApiError> {
    if token.len() > 4096 {
        return Err(unauthorized());
    }
    let (payload, tag) = token.rsplit_once('.').ok_or_else(unauthorized)?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| unauthorized())?;
    state_mac(key, payload)?
        .verify_slice(&tag)
        .map_err(|_| unauthorized())?;
    let mut fields = payload.split('.');
    if fields.next() != Some("v1") {
        return Err(unauthorized());
    }
    let expires: u64 = fields
        .next()
        .ok_or_else(unauthorized)?
        .parse()
        .map_err(|_| unauthorized())?;
    let encoded_state = fields.next().ok_or_else(unauthorized)?;
    if fields.next().is_some() || expires <= now {
        return Err(unauthorized());
    }
    let state = URL_SAFE_NO_PAD
        .decode(encoded_state)
        .map_err(|_| unauthorized())?;
    let state = String::from_utf8(state).map_err(|_| unauthorized())?;
    if state.is_empty() {
        return Err(unauthorized());
    }
    Ok(state)
}

/// Reject ambiguous cookies, including duplicates in separate Cookie headers.
fn find_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ApiError> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let value = header.to_str().map_err(|_| unauthorized())?;
        for pair in value.split(';') {
            if let Some((cookie_name, value)) = pair.trim().split_once('=') {
                if cookie_name == name {
                    if found.is_some() || value.is_empty() {
                        return Err(unauthorized());
                    }
                    found = Some(value);
                }
            }
        }
    }
    Ok(found)
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

fn callback_input(
    query: CallbackQuery,
    headers: &HeaderMap,
    key: &[u8],
    now: u64,
) -> Result<(String, String, String), ApiError> {
    let callback_state = query
        .state
        .filter(|s| !s.is_empty())
        .ok_or_else(unauthorized)?;
    let cookie = find_cookie(headers, LOGIN_COOKIE)?.ok_or_else(unauthorized)?;
    let browser_state = verify_login_state(key, cookie, now)?;
    if browser_state != callback_state {
        return Err(unauthorized());
    }
    if query.error.is_some() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "identity provider denied login",
        ));
    }
    let code = query
        .code
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing authorization code"))?;
    Ok((browser_state, callback_state, code))
}

/// Use only the configured canonical HTTPS origin, never a request-supplied
/// return URL, for redirects back to the application.
fn home_url(origin: &str) -> Result<String, ApiError> {
    let url = url::Url::parse(origin).map_err(|_| internal_error())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.origin().ascii_serialization() != origin
    {
        return Err(internal_error());
    }
    Ok(format!("{origin}/"))
}

fn require_origin(headers: &HeaderMap, origin: &str) -> Result<(), ApiError> {
    let values = headers.get_all(header::ORIGIN);
    let mut values = values.iter();
    let valid = values.next().and_then(|value| value.to_str().ok()) == Some(origin)
        && values.next().is_none();
    if !valid {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "origin rejected"));
    }
    Ok(())
}

fn redirect(status: StatusCode, location: &str) -> Result<Response, ApiError> {
    let location = HeaderValue::from_str(location).map_err(|_| internal_error())?;
    let mut response = status.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    Ok(response)
}

fn protect_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn login_cookie(token: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&format!(
        "{LOGIN_COOKIE}={token}; Path=/; Max-Age={LOGIN_LIFETIME_SECONDS}; Secure; HttpOnly; SameSite=Lax"
    )).map_err(|_| internal_error())
}

fn session_cookie(token: &str) -> Result<HeaderValue, ApiError> {
    // Identity's signed token consists only of a UUID, a dot and base64url.
    // Never let an unexpected token inject cookie attributes.
    if token.is_empty()
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(internal_error());
    }
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Lax"
    ))
    .map_err(|_| internal_error())
}

fn start_response(start: LoginStart, key: &[u8], now: u64) -> Result<Response, ApiError> {
    let authorization_url = url::Url::parse(&start.authorization_url)
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "invalid authorization URL"))?;
    if authorization_url.scheme() != "https"
        || authorization_url.host_str().is_none()
        || !authorization_url.username().is_empty()
        || authorization_url.password().is_some()
        || authorization_url.fragment().is_some()
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "invalid authorization URL",
        ));
    }
    let mut states = authorization_url
        .query_pairs()
        .filter(|(name, _)| name.as_ref() == "state");
    if states
        .next()
        .map(|(_, value)| value.as_ref() == start.state.as_str())
        != Some(true)
        || states.next().is_some()
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "invalid authorization state",
        ));
    }
    let token = sign_login_state(key, &start.state, now)?;
    let mut response = redirect(StatusCode::FOUND, &start.authorization_url)?;
    response
        .headers_mut()
        .append(header::SET_COOKIE, login_cookie(&token)?);
    Ok(response)
}

#[utoipa::path(get, path = "/auth/login", responses((status = 302, description = "Redirect to the OIDC provider"), (status = 503, body = crate::ErrorBody)))]
pub(crate) async fn login(State(app): State<Arc<AppState>>) -> Response {
    let result = async {
        home_url(&app.config.public_origin)?;
        // Check configuration before creating an identity login attempt.
        if app.config.cookie_signing_key.len() < 32 {
            return Err(internal_error());
        }
        let start = app.identity.begin_login().await?;
        start_response(start, &app.config.cookie_signing_key, now_seconds()?)
    }
    .await;
    protect_response(result.unwrap_or_else(IntoResponse::into_response))
}

fn authenticated_response(token: &str, home: &str) -> Result<Response, ApiError> {
    let mut response = redirect(StatusCode::SEE_OTHER, home)?;
    response
        .headers_mut()
        .append(header::SET_COOKIE, session_cookie(token)?);
    Ok(response)
}

fn finish_callback(result: Result<Response, ApiError>) -> Response {
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    // Clear even on an invalid callback or a provider error. Identity consumes
    // successful attempts exactly once, including failed token exchanges.
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(CLEAR_LOGIN_COOKIE),
    );
    protect_response(response)
}

#[utoipa::path(get, path = "/auth/callback", params(("code" = Option<String>, Query), ("state" = Option<String>, Query), ("error" = Option<String>, Query)), responses((status = 303, description = "Authenticated browser redirect"), (status = 400, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody)))]
pub(crate) async fn callback(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    let result = async {
        let Query(query) =
            query.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid callback query"))?;
        let home = home_url(&app.config.public_origin)?;
        let (browser_state, callback_state, code) = callback_input(
            query,
            &headers,
            &app.config.cookie_signing_key,
            now_seconds()?,
        )?;
        let token = app
            .identity
            .complete_login(&browser_state, &callback_state, &code)
            .await?;
        authenticated_response(&token, &home)
    }
    .await;
    finish_callback(result)
}

fn logout_response(outcome: Result<(), IdentityError>, home: &str) -> Result<Response, ApiError> {
    let mut response = match outcome {
        Ok(()) => redirect(StatusCode::SEE_OTHER, home)?,
        // An invalid, expired, revoked or disabled session cannot authenticate;
        // discard its browser cookie while still reporting the auth failure.
        Err(IdentityError::Unauthorized) => {
            ApiError::from(IdentityError::Unauthorized).into_response()
        }
        Err(error) => return Err(error.into()),
    };
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(CLEAR_SESSION_COOKIE),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(CLEAR_LOGIN_COOKIE),
    );
    Ok(response)
}

#[utoipa::path(post, path = "/auth/logout", responses((status = 303, description = "Session revoked; redirect to the UI"), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody)))]
pub(crate) async fn logout(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let result: Result<Response, ApiError> = async {
        let home = home_url(&app.config.public_origin)?;
        require_origin(&headers, &app.config.public_origin)?;
        let session = crate::session(&headers)?;
        logout_response(app.identity.logout(&session).await, &home)
    }
    .await;
    protect_response(result.unwrap_or_else(IntoResponse::into_response))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a sufficiently long cookie signing key";

    fn cookie_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=1; {LOGIN_COOKIE}={token}")).unwrap(),
        );
        headers
    }

    fn query(state: &str, code: &str) -> CallbackQuery {
        CallbackQuery {
            state: Some(state.into()),
            code: Some(code.into()),
            error: None,
        }
    }

    fn status(error: ApiError) -> StatusCode {
        error.into_response().status()
    }

    #[test]
    fn signed_state_is_bound_to_key_content_and_expiration() {
        let token = sign_login_state(KEY, "browser-state", 1_000).unwrap();
        assert_eq!(
            verify_login_state(KEY, &token, 1_599).unwrap(),
            "browser-state"
        );
        assert_eq!(
            status(verify_login_state(KEY, &token, 1_600).unwrap_err()),
            StatusCode::UNAUTHORIZED
        );
        assert!(
            verify_login_state(b"different sufficiently long signing key", &token, 1_001).is_err()
        );
        assert!(verify_login_state(KEY, &token.replace("v1.1600.", "v1.9999."), 1_001).is_err());
        let altered = token.replace(
            &URL_SAFE_NO_PAD.encode("browser-state"),
            &URL_SAFE_NO_PAD.encode("another-state"),
        );
        assert!(verify_login_state(KEY, &altered, 1_001).is_err());
        assert!(verify_login_state(KEY, "v1.9999.Zm9v.invalid", 1_001).is_err());
        assert!(sign_login_state(b"short", "state", 1_000).is_err());
    }

    #[test]
    fn callback_requires_its_own_single_unexpired_browser_cookie() {
        let token = sign_login_state(KEY, "state-one", 100).unwrap();
        let headers = cookie_headers(&token);
        let (browser, callback, code) =
            callback_input(query("state-one", "code"), &headers, KEY, 101).unwrap();
        assert_eq!(
            (browser.as_str(), callback.as_str(), code.as_str()),
            ("state-one", "state-one", "code")
        );
        assert_eq!(
            status(callback_input(query("state-two", "code"), &headers, KEY, 101).unwrap_err()),
            StatusCode::UNAUTHORIZED
        );
        assert!(callback_input(query("state-one", "code"), &HeaderMap::new(), KEY, 101).is_err());
        assert!(callback_input(query("state-one", "code"), &headers, KEY, 700).is_err());
        let mut duplicate = headers.clone();
        duplicate.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{LOGIN_COOKIE}={token}")).unwrap(),
        );
        assert!(callback_input(query("state-one", "code"), &duplicate, KEY, 101).is_err());
        let mut same_header = HeaderMap::new();
        same_header.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{LOGIN_COOKIE}={token}; {LOGIN_COOKIE}={token}"))
                .unwrap(),
        );
        assert!(callback_input(query("state-one", "code"), &same_header, KEY, 101).is_err());
        assert_eq!(
            status(callback_input(query("state-one", ""), &headers, KEY, 101).unwrap_err()),
            StatusCode::BAD_REQUEST
        );
        let denied = CallbackQuery {
            error: Some("access_denied".into()),
            ..query("state-one", "code")
        };
        assert_eq!(
            status(callback_input(denied, &headers, KEY, 101).unwrap_err()),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn callback_query_rejects_duplicates_and_never_controls_the_return_url() {
        let token = sign_login_state(KEY, "state-one", 100).unwrap();
        let headers = cookie_headers(&token);
        let uri: axum::http::Uri =
            "/auth/callback?code=code&state=state-one&return_to=https%3A%2F%2Fevil.example"
                .parse()
                .unwrap();
        let Query(query) = Query::<CallbackQuery>::try_from_uri(&uri).unwrap();
        assert!(callback_input(query, &headers, KEY, 101).is_ok());
        assert_eq!(
            home_url("https://example.com").unwrap(),
            "https://example.com/"
        );
        for uri in [
            "/auth/callback?code=code&state=state-one&state=state-two",
            "/auth/callback?code=one&code=two&state=state-one",
            "/auth/callback?error=one&error=two&state=state-one",
        ] {
            let uri: axum::http::Uri = uri.parse().unwrap();
            assert!(Query::<CallbackQuery>::try_from_uri(&uri).is_err());
        }
    }

    #[test]
    fn redirect_destination_is_only_the_configured_canonical_https_origin() {
        assert_eq!(
            home_url("https://example.com").unwrap(),
            "https://example.com/"
        );
        assert_eq!(
            home_url("https://example.com:8443").unwrap(),
            "https://example.com:8443/"
        );
        for invalid in [
            "http://example.com",
            "https://example.com/",
            "https://example.com/path",
            "https://example.com?next=//evil.example",
            "https://user@example.com",
            "https://example.com#fragment",
            "https://example.com:443",
            "//evil.example",
        ] {
            assert!(home_url(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn logout_requires_one_exact_origin() {
        let mut headers = HeaderMap::new();
        assert!(require_origin(&headers, "https://example.com").is_err());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://example.com"),
        );
        assert!(require_origin(&headers, "https://example.com").is_ok());
        headers.append(
            header::ORIGIN,
            HeaderValue::from_static("https://example.com"),
        );
        assert!(require_origin(&headers, "https://example.com").is_err());
        for invalid in [
            "null",
            "https://evil.example",
            "https://example.com.evil.example",
            "https://example.com:444",
            "https://example.com https://evil.example",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_str(invalid).unwrap());
            assert!(
                require_origin(&headers, "https://example.com").is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn login_redirect_sets_short_lived_secure_cookie_and_checks_provider_state() {
        let start = LoginStart {
            authorization_url: "https://idp.example/authorize?client_id=app&state=browser-state"
                .into(),
            state: "browser-state".into(),
        };
        let response = protect_response(start_response(start, KEY, 1_000).unwrap());
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://idp.example/authorize?client_id=app&state=browser-state"
        );
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("__Host-thalamus-oidc="));
        assert!(cookie.contains("; Path=/; Max-Age=600; Secure; HttpOnly; SameSite=Lax"));
        assert!(!cookie.contains("Domain="));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        for url in [
            "http://idp.example/authorize?state=browser-state",
            "https://idp.example/authorize?state=other",
            "https://idp.example/authorize?state=browser-state&state=other",
            "https://user@idp.example/authorize?state=browser-state",
        ] {
            let start = LoginStart {
                authorization_url: url.into(),
                state: "browser-state".into(),
            };
            assert!(start_response(start, KEY, 1_000).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn logout_only_clears_browser_cookies_after_revocation_or_invalid_session() {
        let home = "https://example.com/";
        let success = logout_response(Ok(()), home).unwrap();
        assert_eq!(success.status(), StatusCode::SEE_OTHER);
        assert_eq!(success.headers()[header::LOCATION], home);
        let cookies: Vec<_> = success
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            cookies.as_slice(),
            &[CLEAR_SESSION_COOKIE, CLEAR_LOGIN_COOKIE]
        );

        let invalid = logout_response(Err(IdentityError::Unauthorized), home).unwrap();
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
        let cookies: Vec<_> = invalid
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            cookies.as_slice(),
            &[CLEAR_SESSION_COOKIE, CLEAR_LOGIN_COOKIE]
        );

        let unavailable = logout_response(
            Err(IdentityError::Database(sqlx::Error::PoolTimedOut)),
            home,
        );
        assert_eq!(
            status(unavailable.err().unwrap()),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn session_cookie_and_callback_cleanup_are_secure() {
        let cookie = session_cookie("abc-123.signature_456").unwrap();
        assert_eq!(
            cookie,
            "__Host-thalamus-session=abc-123.signature_456; Path=/; Secure; HttpOnly; SameSite=Lax"
        );
        assert!(session_cookie("bad; Domain=evil.example").is_err());
        assert!(session_cookie("").is_err());
        let success = finish_callback(authenticated_response(
            "abc-123.signature_456",
            "https://example.com/",
        ));
        let cookies: Vec<_> = success
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies.len(), 2);
        assert!(cookies[0].starts_with("__Host-thalamus-session="));
        assert_eq!(cookies[1], CLEAR_LOGIN_COOKIE);
        let failure = finish_callback(Err(unauthorized()));
        assert_eq!(failure.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(failure.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(failure.headers()[header::SET_COOKIE], CLEAR_LOGIN_COOKIE);
        assert_eq!(failure.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            CLEAR_SESSION_COOKIE.contains("; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax")
        );
    }
}
