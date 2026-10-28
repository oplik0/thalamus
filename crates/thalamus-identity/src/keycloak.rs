//! Complete, checked Keycloak Admin REST observations.
//!
//! The confidential client needs the `realm-management` client role
//! `view-users` in its service-account access token, granting realm-wide user
//! and group visibility. Configure the application's OIDC group-membership
//! mapper with `full.path=true`: these are the exact `path` values returned by
//! the Admin API. Inconsistent scans and limit violations fail closed.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, Url};
use serde::{de::DeserializeOwned, Deserialize};

use super::{Error, IdpSnapshot, IdpSubjectSnapshot, Result};

const PAGE_SIZE: usize = 100;
const MAX_USERS: usize = 100_000;
const MAX_GROUPS_PER_USER: usize = 10_000;
const MAX_GROUP_MEMBERSHIPS: usize = 200_000;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SCAN_TIME: Duration = Duration::from_secs(10 * 60);

/// Credentials for a confidential Keycloak client with service accounts enabled.
/// `issuer` must be the exact canonical HTTPS OIDC issuer, and `ca_pem`, when
/// supplied, is a PEM certificate bundle added to the built-in public roots.
pub struct KeycloakConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub ca_pem: Option<Vec<u8>>,
}

/// Collects authoritative snapshots from the configured Keycloak realm.
/// Neither this type nor its configuration implements `Debug`, to avoid
/// accidentally exposing the client secret.
pub struct KeycloakCollector {
    client: Client,
    issuer: String,
    client_id: String,
    client_secret: String,
    token_url: Url,
    users_url: Url,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
}

#[derive(Deserialize)]
struct TokenClaims {
    resource_access: ResourceAccess,
}

#[derive(Deserialize)]
struct ResourceAccess {
    #[serde(rename = "realm-management")]
    realm_management: RealmManagementAccess,
}

#[derive(Deserialize)]
struct RealmManagementAccess {
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct UserRepresentation {
    id: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct GroupRepresentation {
    path: String,
}

#[derive(Deserialize)]
struct GroupCount {
    count: u64,
}

// Keycloak's user count can itself be filtered by fine-grained permissions.
// A matching list and count therefore prove completeness only when the
// service-account token explicitly grants global view-users. Inspect only the
// token just obtained over the configured HTTPS connection; authorization of
// that same bearer token remains the responsibility of the Keycloak server.
fn require_global_view(token: &str) -> Result<()> {
    let denied = || Error::Oidc("Keycloak token lacks realm-management view-users".into());
    let mut segments = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(denied());
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(denied());
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| denied())?;
    let claims: TokenClaims = serde_json::from_slice(&payload).map_err(|_| denied())?;
    if !claims
        .resource_access
        .realm_management
        .roles
        .iter()
        .any(|role| role == "view-users")
    {
        return Err(denied());
    }
    Ok(())
}

fn invalid_issuer() -> Error {
    Error::Invalid("Keycloak issuer must be a canonical HTTPS realm URL")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// Url::path_segments returns encoded segments. Decode each one before using
// path_segments_mut().push(), which deliberately encodes '%' and '/' itself.
fn decode_issuer_segment(segment: &str) -> Result<String> {
    let input = segment.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let high = input.get(i + 1).and_then(|byte| hex_value(*byte));
            let low = input.get(i + 2).and_then(|byte| hex_value(*byte));
            match (high, low) {
                (Some(high), Some(low)) => output.push(high * 16 + low),
                _ => return Err(invalid_issuer()),
            }
            i += 3;
        } else {
            output.push(input[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(output).map_err(|_| invalid_issuer())?;
    if decoded.is_empty()
        || decoded == "."
        || decoded == ".."
        || decoded.contains('/')
        || decoded.contains('\\')
        || decoded.chars().any(char::is_control)
    {
        return Err(invalid_issuer());
    }
    Ok(decoded)
}

fn append_segments(base: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    {
        let mut path = url.path_segments_mut().map_err(|_| invalid_issuer())?;
        for segment in segments {
            // The URL library intentionally ignores dot segments. Do not let an
            // ID change which resource is requested, even if it is unusual.
            if segment.is_empty() || *segment == "." || *segment == ".." {
                return Err(Error::Invalid("invalid Keycloak URL path segment"));
            }
            path.push(segment);
        }
    }
    Ok(url)
}

fn realm_endpoints(issuer: &str) -> Result<(Url, Url)> {
    let url = Url::parse(issuer).map_err(|_| invalid_issuer())?;
    if url.as_str() != issuer
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_issuer());
    }
    let segments: Vec<String> = url
        .path_segments()
        .ok_or_else(invalid_issuer)?
        .map(decode_issuer_segment)
        .collect::<Result<_>>()?;
    if segments.len() < 2 || segments[segments.len() - 2] != "realms" {
        return Err(invalid_issuer());
    }

    // Require a unique segment spelling as well as URL parser canonicality.
    // This rejects encoded separators, malformed escapes, and alternate escapes
    // of unreserved characters, while retaining encoded non-ASCII realm names.
    let mut reconstructed = url.clone();
    reconstructed
        .path_segments_mut()
        .map_err(|_| invalid_issuer())?
        .clear()
        .extend(segments.iter().map(String::as_str));
    if reconstructed != url {
        return Err(invalid_issuer());
    }

    let token_url = append_segments(&url, &["protocol", "openid-connect", "token"])?;
    let mut users_url = url.clone();
    {
        let mut path = users_url
            .path_segments_mut()
            .map_err(|_| invalid_issuer())?;
        path.pop()
            .pop()
            .push("admin")
            .push("realms")
            .push(&segments[segments.len() - 1])
            .push("users");
    }
    Ok((token_url, users_url))
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8], operation: &'static str) -> Result<T> {
    // Never include a response body or serde error: either can contain secrets.
    serde_json::from_slice(bytes)
        .map_err(|_| Error::Oidc(format!("invalid Keycloak {operation} response")))
}

fn scan_timeout(started: Instant) -> Result<Duration> {
    let remaining = MAX_SCAN_TIME
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| Error::Oidc("Keycloak snapshot exceeded ten minutes".into()))?;
    Ok(remaining.min(REQUEST_TIMEOUT))
}

fn ensure_fresh(started: Instant, observed_at: DateTime<Utc>) -> Result<()> {
    let now = Utc::now();
    if started.elapsed() >= MAX_SCAN_TIME
        || now < observed_at
        || now.signed_duration_since(observed_at) >= chrono::Duration::minutes(10)
    {
        return Err(Error::Oidc(
            "Keycloak snapshot exceeded ten minutes or clock moved backwards".into(),
        ));
    }
    Ok(())
}

// Explicit enabled filters include service-account users, which Keycloak's
// unfiltered users endpoint can omit.
#[derive(Clone, Copy, PartialEq, Eq)]
struct UserCounts {
    enabled: usize,
    disabled: usize,
}

#[derive(Default)]
struct ScanUsage {
    text_bytes: usize,
    group_memberships: usize,
}

impl ScanUsage {
    fn add_text(&mut self, bytes: usize) -> Result<()> {
        self.text_bytes = self
            .text_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_SNAPSHOT_BYTES)
            .ok_or(Error::Invalid("Keycloak snapshot is too large"))?;
        Ok(())
    }

    fn add_group(&mut self, bytes: usize) -> Result<()> {
        if self.group_memberships >= MAX_GROUP_MEMBERSHIPS {
            return Err(Error::Invalid("too many Keycloak group memberships"));
        }
        self.add_text(bytes)?;
        self.group_memberships += 1;
        Ok(())
    }
}

fn add_users_page(
    users: &mut BTreeMap<String, bool>,
    page: Vec<UserRepresentation>,
    expected: usize,
    enabled: bool,
    usage: &mut ScanUsage,
) -> Result<()> {
    if page.len() > PAGE_SIZE || page.len() > expected.saturating_sub(users.len()) {
        return Err(Error::Oidc(
            "Keycloak user page exceeds the reported count or page size".into(),
        ));
    }
    for user in page {
        if user.enabled != enabled {
            return Err(Error::Oidc(
                "Keycloak user does not match enabled filter".into(),
            ));
        }
        if user.id.is_empty()
            || user.id.len() > 255
            || user.id == "."
            || user.id == ".."
            || user.id.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid Keycloak user ID"));
        }
        if users.contains_key(&user.id) {
            return Err(Error::Invalid("duplicate Keycloak user ID"));
        }
        usage.add_text(user.id.len())?;
        users.insert(user.id, user.enabled);
    }
    Ok(())
}

fn add_groups_page(
    groups: &mut BTreeSet<String>,
    page: Vec<GroupRepresentation>,
    expected: usize,
    usage: &mut ScanUsage,
) -> Result<()> {
    if page.len() > PAGE_SIZE || page.len() > expected.saturating_sub(groups.len()) {
        return Err(Error::Oidc(
            "Keycloak group page exceeds the reported count or page size".into(),
        ));
    }
    for group in page {
        if !group.path.starts_with('/')
            || group.path.len() > 1024
            || group.path.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid Keycloak group path"));
        }
        if groups.contains(&group.path) {
            return Err(Error::Invalid("duplicate Keycloak group path"));
        }
        usage.add_group(group.path.len())?;
        groups.insert(group.path);
    }
    Ok(())
}

// Verify the exact serialized size without allocating a second snapshot-sized
// buffer. This matches the downstream reconciliation limit, including JSON
// escaping and structure overhead.
struct SizeLimitedWriter {
    remaining: usize,
}

impl Write for SizeLimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("snapshot size limit"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_snapshot_size(snapshot: &IdpSnapshot) -> Result<()> {
    let mut writer = SizeLimitedWriter {
        remaining: MAX_SNAPSHOT_BYTES,
    };
    serde_json::to_writer(&mut writer, snapshot)
        .map_err(|_| Error::Invalid("Keycloak snapshot is too large"))
}

impl KeycloakCollector {
    pub fn new(config: KeycloakConfig) -> Result<Self> {
        if config.client_id.is_empty() || config.client_secret.is_empty() {
            return Err(Error::Invalid("Keycloak client ID and secret are required"));
        }
        if config.issuer.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Invalid("Keycloak issuer is too long"));
        }
        let (token_url, users_url) = realm_endpoints(&config.issuer)?;
        let mut builder = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT);
        if let Some(pem) = config.ca_pem.as_deref() {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| Error::Invalid("invalid Keycloak CA PEM bundle"))?;
            if certificates.is_empty() {
                return Err(Error::Invalid("invalid Keycloak CA PEM bundle"));
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let client = builder
            .build()
            .map_err(|_| Error::Oidc("cannot construct Keycloak HTTPS client".into()))?;
        Ok(Self {
            client,
            issuer: config.issuer,
            client_id: config.client_id,
            client_secret: config.client_secret,
            token_url,
            users_url,
        })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        mut request: RequestBuilder,
        operation: &'static str,
        started: Option<Instant>,
    ) -> Result<T> {
        if let Some(started) = started {
            request = request.timeout(scan_timeout(started)?);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| Error::Oidc(format!("Keycloak {operation} request failed")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Oidc(format!(
                "Keycloak {operation} returned HTTP {}",
                status.as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Error::Oidc(format!(
                "Keycloak {operation} response is too large"
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Oidc(format!("Keycloak {operation} response failed")))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(Error::Oidc(format!(
                    "Keycloak {operation} response is too large"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        decode_json(&body, operation)
    }

    async fn access_token(&self) -> Result<String> {
        let request = self.client.post(self.token_url.clone()).form(&[
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ]);
        let token: TokenResponse = self.json(request, "token", None).await?;
        if token.access_token.is_empty()
            || token.access_token.chars().any(char::is_control)
            || !token.token_type.eq_ignore_ascii_case("bearer")
        {
            return Err(Error::Oidc("invalid Keycloak token response".into()));
        }
        require_global_view(&token.access_token)?;
        Ok(token.access_token)
    }

    async fn user_count(&self, token: &str, enabled: bool, started: Instant) -> Result<usize> {
        let url = append_segments(&self.users_url, &["count"])?;
        let request = self
            .client
            .get(url)
            .bearer_auth(token)
            .query(&[("enabled", enabled)]);
        let count: u64 = self.json(request, "user count", Some(started)).await?;
        if count > MAX_USERS as u64 {
            return Err(Error::Invalid("too many Keycloak users"));
        }
        Ok(count as usize)
    }

    async fn user_counts(&self, token: &str, started: Instant) -> Result<UserCounts> {
        let enabled = self.user_count(token, true, started).await?;
        let disabled = self.user_count(token, false, started).await?;
        if enabled + disabled > MAX_USERS {
            return Err(Error::Invalid("too many Keycloak users"));
        }
        Ok(UserCounts { enabled, disabled })
    }

    async fn group_count(&self, url: &Url, token: &str, started: Instant) -> Result<usize> {
        let request = self.client.get(url.clone()).bearer_auth(token);
        let count: GroupCount = self.json(request, "group count", Some(started)).await?;
        if count.count > MAX_GROUPS_PER_USER as u64 {
            return Err(Error::Invalid("too many Keycloak groups for a user"));
        }
        Ok(count.count as usize)
    }

    async fn scan_groups(
        &self,
        token: &str,
        subject: &str,
        started: Instant,
        usage: &mut ScanUsage,
    ) -> Result<Vec<String>> {
        let url = append_segments(&self.users_url, &[subject, "groups"])?;
        let count_url = append_segments(&url, &["count"])?;
        // Keycloak may filter groups the caller cannot view from the list, but
        // its documented count endpoint reports all of the user's memberships.
        let before = self.group_count(&count_url, token, started).await?;
        if before > MAX_GROUP_MEMBERSHIPS.saturating_sub(usage.group_memberships) {
            return Err(Error::Invalid("too many Keycloak group memberships"));
        }
        let mut groups = BTreeSet::new();
        loop {
            let request = self
                .client
                .get(url.clone())
                .bearer_auth(token)
                .query(&[("first", groups.len()), ("max", PAGE_SIZE)]);
            let page: Vec<GroupRepresentation> =
                self.json(request, "groups", Some(started)).await?;
            if page.is_empty() {
                break;
            }
            add_groups_page(&mut groups, page, before, usage)?;
        }
        let after = self.group_count(&count_url, token, started).await?;
        if before != after || groups.len() != before {
            return Err(Error::Oidc(
                "incomplete or changing Keycloak group memberships".into(),
            ));
        }
        Ok(groups.into_iter().collect())
    }

    async fn scan(
        &self,
        token: &str,
        expected: UserCounts,
        started: Instant,
    ) -> Result<Vec<IdpSubjectSnapshot>> {
        let mut users = BTreeMap::new();
        let mut usage = ScanUsage::default();
        let total = expected.enabled + expected.disabled;
        for (enabled, partition_count) in [(true, expected.enabled), (false, expected.disabled)] {
            let mut first = 0;
            loop {
                let request = self
                    .client
                    .get(self.users_url.clone())
                    .bearer_auth(token)
                    .query(&[("first", first), ("max", PAGE_SIZE)])
                    .query(&[("enabled", enabled)])
                    .query(&[("briefRepresentation", "true")]);
                let page: Vec<UserRepresentation> =
                    self.json(request, "users", Some(started)).await?;
                if page.is_empty() {
                    break;
                }
                let page_len = page.len();
                if page_len > partition_count.saturating_sub(first) {
                    return Err(Error::Oidc(
                        "Keycloak user page exceeds the reported partition count".into(),
                    ));
                }
                add_users_page(&mut users, page, total, enabled, &mut usage)?;
                first += page_len;
            }
            if first != partition_count {
                return Err(Error::Oidc("incomplete Keycloak user list".into()));
            }
        }
        let mut subjects = Vec::with_capacity(users.len());
        for (subject, active) in users {
            let groups = if active {
                self.scan_groups(token, &subject, started, &mut usage)
                    .await?
            } else {
                Vec::new()
            };
            subjects.push(IdpSubjectSnapshot {
                subject,
                active,
                groups,
            });
        }
        Ok(subjects)
    }

    /// Read two complete scans and publish only if user counts, identities,
    /// enabled states, and every active user's exact group paths agree. The
    /// timestamp is the beginning of the first scan, not its completion.
    pub async fn snapshot(&self) -> Result<IdpSnapshot> {
        let token = self.access_token().await?;
        let started = Instant::now();
        let observed_at = Utc::now();
        let before = self.user_counts(&token, started).await?;
        let first = self.scan(&token, before, started).await?;
        let middle = self.user_counts(&token, started).await?;
        if before != middle {
            return Err(Error::Oidc(
                "Keycloak user count changed during snapshot".into(),
            ));
        }
        let second = self.scan(&token, before, started).await?;
        let after = self.user_counts(&token, started).await?;
        if before != after || first != second {
            return Err(Error::Oidc(
                "Keycloak identities or memberships changed during snapshot".into(),
            ));
        }
        let snapshot = IdpSnapshot {
            issuer: self.issuer.clone(),
            observed_at,
            subjects: first,
        };
        check_snapshot_size(&snapshot)?;
        ensure_fresh(started, observed_at)?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_global_realm_management_view_role_in_token() {
        let token = |payload: &str| format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(payload));
        assert!(require_global_view(&token(
            r#"{"resource_access":{"realm-management":{"roles":["view-users"]}}}"#
        ))
        .is_ok());
        for claims in [
            r#"{}"#,
            r#"{"resource_access":{"realm-management":{"roles":[]}}}"#,
            r#"{"resource_access":{"realm-management":{"roles":["query-users"]}}}"#,
            r#"{"resource_access":{"another-client":{"roles":["view-users"]}}}"#,
        ] {
            assert!(require_global_view(&token(claims)).is_err());
        }
        assert!(require_global_view("opaque-token").is_err());
        assert!(require_global_view("e30.e30.").is_err());
        assert!(require_global_view("e30.e30.signature.extra").is_err());
    }

    #[test]
    fn derives_urls_from_canonical_issuer_segments() {
        let (token, users) = realm_endpoints("https://id.example/auth/realms/team%20one").unwrap();
        assert_eq!(
            token.as_str(),
            "https://id.example/auth/realms/team%20one/protocol/openid-connect/token"
        );
        assert_eq!(
            users.as_str(),
            "https://id.example/auth/admin/realms/team%20one/users"
        );
        let groups = append_segments(&users, &["f:storage:member/one", "groups"]).unwrap();
        assert_eq!(
            groups.as_str(),
            "https://id.example/auth/admin/realms/team%20one/users/f:storage:member%2Fone/groups"
        );
        assert!(append_segments(&users, &["..", "groups"]).is_err());
        assert!(append_segments(&users, &[".", "groups"]).is_err());
    }

    #[test]
    fn rejects_noncanonical_or_unsafe_issuers() {
        for issuer in [
            "http://id.example/realms/team",
            "HTTPS://id.example/realms/team",
            "https://ID.example/realms/team",
            "https://id.example:443/realms/team",
            "https://user@id.example/realms/team",
            "https://user:password@id.example/realms/team",
            "https://id.example/realms/team?",
            "https://id.example/realms/team#",
            "https://id.example/realms/team/",
            "https://id.example/realms/",
            "https://id.example/realms/team/extra",
            "https://id.example//realms/team",
            "https://id.example/realms/a%2Fb",
            "https://id.example/realms/a%5Cb",
            "https://id.example/realms/%2e%2e",
            "https://id.example/realms/%41",
            "https://id.example/realms/bad%xx",
        ] {
            assert!(realm_endpoints(issuer).is_err(), "accepted {issuer}");
        }
    }

    #[test]
    fn strictly_parses_users_and_rejects_duplicates_across_pages() {
        for malformed in [
            &br#"[{"enabled":true}]"#[..],
            &br#"[{"id":"u"}]"#[..],
            &br#"[{"id":"u","enabled":null}]"#[..],
            &br#"[{"id":"u","enabled":"true"}]"#[..],
        ] {
            assert!(decode_json::<Vec<UserRepresentation>>(malformed, "users").is_err());
        }
        let mut users = BTreeMap::new();
        let mut usage = ScanUsage::default();
        let page = decode_json(br#"[{"id":"u","enabled":false}]"#, "users").unwrap();
        add_users_page(&mut users, page, 2, false, &mut usage).unwrap();
        assert_eq!(users.get("u"), Some(&false));
        let duplicate = decode_json(br#"[{"id":"u","enabled":true}]"#, "users").unwrap();
        assert!(add_users_page(&mut users, duplicate, 2, true, &mut usage).is_err());
        let empty = decode_json(br#"[{"id":"","enabled":true}]"#, "users").unwrap();
        assert!(add_users_page(
            &mut BTreeMap::new(),
            empty,
            1,
            true,
            &mut ScanUsage::default()
        )
        .is_err());
        let wrong_partition = decode_json(br#"[{"id":"u2","enabled":false}]"#, "users").unwrap();
        assert!(add_users_page(
            &mut BTreeMap::new(),
            wrong_partition,
            1,
            true,
            &mut ScanUsage::default()
        )
        .is_err());
    }

    #[test]
    fn requires_exact_full_group_paths_and_rejects_duplicates() {
        for malformed in [&br#"[{"name":"staff"}]"#[..], &br#"[{"path":null}]"#[..]] {
            assert!(decode_json::<Vec<GroupRepresentation>>(malformed, "groups").is_err());
        }
        let mut groups = BTreeSet::new();
        let mut usage = ScanUsage::default();
        let page = decode_json(br#"[{"path":"/smoke-admin","name":"ignored"}]"#, "groups").unwrap();
        add_groups_page(&mut groups, page, 2, &mut usage).unwrap();
        assert!(groups.contains("/smoke-admin"));
        let duplicate = decode_json(br#"[{"path":"/smoke-admin"}]"#, "groups").unwrap();
        assert!(add_groups_page(&mut groups, duplicate, 2, &mut usage).is_err());
        for path in ["", "staff", "/bad\npath"] {
            let page = vec![GroupRepresentation { path: path.into() }];
            assert!(
                add_groups_page(&mut BTreeSet::new(), page, 1, &mut ScanUsage::default()).is_err()
            );
        }
    }

    #[test]
    fn parses_counts_without_defaults_and_enforces_page_bounds() {
        assert_eq!(decode_json::<u64>(b"2", "user count").unwrap(), 2);
        assert!(decode_json::<u64>(b"-1", "user count").is_err());
        assert!(decode_json::<GroupCount>(br#"{}"#, "group count").is_err());
        assert_eq!(
            decode_json::<GroupCount>(br#"{"count":3}"#, "group count")
                .unwrap()
                .count,
            3
        );
        let page = vec![UserRepresentation {
            id: "u".into(),
            enabled: true,
        }];
        assert!(add_users_page(
            &mut BTreeMap::new(),
            page,
            0,
            true,
            &mut ScanUsage::default()
        )
        .is_err());
        let page = (0..=PAGE_SIZE)
            .map(|i| UserRepresentation {
                id: i.to_string(),
                enabled: true,
            })
            .collect();
        assert!(add_users_page(
            &mut BTreeMap::new(),
            page,
            PAGE_SIZE + 1,
            true,
            &mut ScanUsage::default()
        )
        .is_err());
    }
}
