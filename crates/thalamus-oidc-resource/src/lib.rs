//! OAuth resource-server authentication via a configured trusted introspection endpoint.
//!
//! Successful authentication identifies an external subject, not local authorization. Callers
//! must resolve that issuer/subject to a local account and load its current roles on each request.

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// Configuration is deliberately not `Debug`: it contains a client secret.
pub struct ResourceConfig {
    pub introspection_endpoint: String,
    pub client_id: String,
    pub client_secret: String,
    pub issuer: String,
    pub audience: String,
    pub required_scopes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedSubject {
    pub issuer: String,
    pub subject: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("invalid resource server configuration")]
    Configuration,
    #[error("invalid bearer credentials")]
    Unauthorized,
    #[error("token introspection unavailable")]
    Unavailable,
}

/// RFC 9728 protected resource metadata. Serve at the appropriate well-known URL.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
}

pub struct ResourceVerifier {
    config: ResourceConfig,
    client: reqwest::Client,
}

fn https_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
    })
}

fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
}

/// Parse all Authorization field values. Reject duplicates and non-bearer credentials.
/// The returned token borrows the supplied header; do not log either value.
pub fn bearer_token<'a>(
    headers: impl IntoIterator<Item = &'a str>,
) -> Result<&'a str, VerifyError> {
    let mut headers = headers.into_iter();
    let header = headers.next().ok_or(VerifyError::Unauthorized)?;
    if headers.next().is_some() {
        return Err(VerifyError::Unauthorized);
    }
    let (scheme, token) = header.split_once(' ').ok_or(VerifyError::Unauthorized)?;
    if !scheme.eq_ignore_ascii_case("bearer") || !valid_token(token) {
        return Err(VerifyError::Unauthorized);
    }
    Ok(token)
}

// RFC 6750 b64token, including opaque tokens, with padding only at the end.
fn valid_token(token: &str) -> bool {
    let mut padding = false;
    let mut payload = false;
    for b in token.bytes() {
        if b == b'=' {
            padding = true;
        } else if !padding
            && (b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/'))
        {
            payload = true;
        } else {
            return false;
        }
    }
    payload
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Introspection {
    active: bool,
    iss: Option<String>,
    sub: Option<String>,
    aud: Option<Audience>,
    exp: Option<u64>,
    scope: Option<String>,
}

impl ResourceVerifier {
    pub fn new(config: ResourceConfig) -> Result<Self, VerifyError> {
        if !https_url(&config.introspection_endpoint)
            || !https_url(&config.issuer)
            || !https_url(&config.audience)
            || config.client_id.is_empty()
            || config.client_secret.is_empty()
            || !config
                .required_scopes
                .iter()
                .all(|scope| valid_scope(scope))
        {
            return Err(VerifyError::Configuration);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| VerifyError::Configuration)?;
        Ok(Self { config, client })
    }

    pub fn metadata(&self) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: self.config.audience.clone(),
            authorization_servers: vec![self.config.issuer.clone()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: self.config.required_scopes.clone(),
        }
    }

    /// Introspect on every request. `audience` must match the configured resource exactly.
    /// The endpoint authenticates the token; unverified local JWT parsing is never used.
    pub async fn verify(
        &self,
        bearer: &str,
        audience: &str,
    ) -> Result<AuthenticatedSubject, VerifyError> {
        if audience != self.config.audience || !valid_token(bearer) {
            return Err(VerifyError::Unauthorized);
        }
        // RFC 6749 section 2.3.1 requires form-encoding each credential before Basic auth.
        let id: String =
            url::form_urlencoded::byte_serialize(self.config.client_id.as_bytes()).collect();
        let secret: String =
            url::form_urlencoded::byte_serialize(self.config.client_secret.as_bytes()).collect();
        let mut response = self
            .client
            .post(&self.config.introspection_endpoint)
            .basic_auth(id, Some(secret))
            .form(&[("token", bearer), ("token_type_hint", "access_token")])
            .send()
            .await
            .map_err(|_| VerifyError::Unavailable)?;
        if !response.status().is_success() {
            return Err(VerifyError::Unavailable);
        }
        const MAX_RESPONSE: usize = 64 * 1024;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(VerifyError::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| VerifyError::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE - bytes.len() {
                return Err(VerifyError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let claims: Introspection =
            serde_json::from_slice(&bytes).map_err(|_| VerifyError::Unauthorized)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| VerifyError::Unavailable)?
            .as_secs();
        self.validate(claims, now)
    }

    fn validate(
        &self,
        claims: Introspection,
        now: u64,
    ) -> Result<AuthenticatedSubject, VerifyError> {
        let audience_matches = match claims.aud {
            Some(Audience::One(aud)) => aud == self.config.audience,
            Some(Audience::Many(aud)) => aud.iter().any(|a| a == &self.config.audience),
            None => false,
        };
        let scope = claims.scope.as_deref().unwrap_or("");
        if !scope.is_empty() && !scope.split(' ').all(valid_scope) {
            return Err(VerifyError::Unauthorized);
        }
        let has_scopes = self
            .config
            .required_scopes
            .iter()
            .all(|required| scope.split(' ').any(|s| s == required));
        let subject = claims
            .sub
            .filter(|s| !s.trim().is_empty())
            .ok_or(VerifyError::Unauthorized)?;
        if !claims.active
            || claims.iss.as_deref() != Some(self.config.issuer.as_str())
            || !audience_matches
            || claims.exp.is_none_or(|exp| exp <= now)
            || !has_scopes
        {
            return Err(VerifyError::Unauthorized);
        }
        Ok(AuthenticatedSubject {
            issuer: self.config.issuer.clone(),
            subject,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn config() -> ResourceConfig {
        ResourceConfig {
            introspection_endpoint: "https://issuer.example/introspect".into(),
            client_id: "client".into(),
            client_secret: "secret".into(),
            issuer: "https://issuer.example".into(),
            audience: "https://api.example/mcp".into(),
            required_scopes: vec!["read".into()],
        }
    }
    fn claims() -> Value {
        json!({"active":true,"iss":"https://issuer.example","sub":"person","aud":["other","https://api.example/mcp"],"exp":101,"scope":"read write"})
    }
    fn check(v: &ResourceVerifier, value: Value) -> Result<AuthenticatedSubject, VerifyError> {
        let parsed = serde_json::from_value(value).map_err(|_| VerifyError::Unauthorized)?;
        v.validate(parsed, 100)
    }
    #[test]
    fn validates_claim_boundaries() {
        let v = ResourceVerifier::new(config()).unwrap();
        assert_eq!(check(&v, claims()).unwrap().subject, "person");
        for (key, value) in [
            ("active", json!(false)),
            ("iss", json!("https://issuer.example/")),
            ("sub", json!("  ")),
            ("aud", json!("https://api.example/mcp/")),
            ("exp", json!(100)),
            ("scope", json!("reader")),
        ] {
            let mut c = claims();
            c[key] = value;
            assert_eq!(check(&v, c), Err(VerifyError::Unauthorized), "{key}");
        }
        let mut c = claims();
        c["aud"] = json!("https://api.example/mcp");
        assert!(check(&v, c).is_ok());
        for key in ["active", "iss", "sub", "aud", "exp", "scope"] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(key);
            assert_eq!(check(&v, c), Err(VerifyError::Unauthorized), "{key}");
        }
        for value in [json!(null), json!("101"), json!(-1), json!(101.1)] {
            let mut c = claims();
            c["exp"] = value;
            assert_eq!(check(&v, c), Err(VerifyError::Unauthorized));
        }
        let mut c = claims();
        c["scope"] = json!("read  write");
        assert_eq!(check(&v, c), Err(VerifyError::Unauthorized));
    }
    #[test]
    fn rejects_bad_authorization_headers() {
        assert_eq!(bearer_token(["Bearer abc.DEF-_/+=="]), Ok("abc.DEF-_/+=="));
        assert_eq!(bearer_token(["bearer abc"]), Ok("abc"));
        for headers in [
            vec![],
            vec!["Basic abc"],
            vec!["Bearer"],
            vec!["Bearer "],
            vec!["Bearer abc,def"],
            vec!["Bearer a=b"],
            vec!["Bearer abc", "Bearer def"],
            vec!["Bearer abc def"],
        ] {
            assert_eq!(bearer_token(headers), Err(VerifyError::Unauthorized));
        }
    }
    #[test]
    fn rejects_untrusted_configuration_and_exposes_metadata() {
        let mut c = config();
        c.introspection_endpoint = "http://issuer.example/introspect".into();
        assert!(matches!(
            ResourceVerifier::new(c),
            Err(VerifyError::Configuration)
        ));
        let mut c = config();
        c.issuer = "https://user:password@issuer.example".into();
        assert!(matches!(
            ResourceVerifier::new(c),
            Err(VerifyError::Configuration)
        ));
        let mut c = config();
        c.required_scopes = vec!["read write".into()];
        assert!(matches!(
            ResourceVerifier::new(c),
            Err(VerifyError::Configuration)
        ));
        let v = ResourceVerifier::new(config()).unwrap();
        assert_eq!(v.metadata().resource, "https://api.example/mcp");
        assert_eq!(
            v.metadata().authorization_servers,
            ["https://issuer.example"]
        );
    }
}
