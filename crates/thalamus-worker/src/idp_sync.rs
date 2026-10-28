//! Worker-only synchronization from a trusted, complete IdP observation.
//!
//! A mounted snapshot is operator supplied, not a request input. Its producer
//! must publish complete observations atomically; a syntactically valid file
//! cannot prove that omitted subjects were actually absent from the IdP.
use std::{
    env,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
};

use thalamus_identity::{
    keycloak::{KeycloakCollector, KeycloakConfig},
    GroupMapping, IdpSnapshot, WorkerIdentity,
};
use tokio::io::AsyncReadExt;

use crate::Result;

const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CA_BYTES: u64 = 1024 * 1024;

// Keep environment values out of Debug and error messages, especially secrets.
#[derive(Default)]
struct Environment {
    client_id: Option<OsString>,
    client_secret: Option<OsString>,
    snapshot_file: Option<OsString>,
    issuer: Option<OsString>,
    mapping: Option<OsString>,
    ca_file: Option<OsString>,
}

impl Environment {
    fn read() -> Self {
        Self {
            client_id: env::var_os("THALAMUS_IDP_KEYCLOAK_CLIENT_ID"),
            client_secret: env::var_os("THALAMUS_IDP_KEYCLOAK_CLIENT_SECRET"),
            snapshot_file: env::var_os("THALAMUS_IDP_SNAPSHOT_FILE"),
            issuer: env::var_os("THALAMUS_OIDC_ISSUER"),
            mapping: env::var_os("THALAMUS_GROUP_MAPPING"),
            ca_file: env::var_os("THALAMUS_OIDC_CA_FILE"),
        }
    }
}

enum ConfiguredSource {
    Keycloak {
        client_id: String,
        client_secret: String,
    },
    SnapshotFile(PathBuf),
}

/// An opted-in source and its operator-configured identity authority.
pub struct IdentitySync {
    issuer: String,
    mapping: GroupMapping,
    keycloak: Option<KeycloakCollector>,
    snapshot_file: Option<PathBuf>,
}

impl IdentitySync {
    /// Configure the bridge once at worker startup. No source means opt-out;
    /// any present but incomplete source configuration is an error.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_values(Environment::read())
    }

    fn from_values(values: Environment) -> Result<Option<Self>> {
        let source = match (values.client_id, values.client_secret, values.snapshot_file) {
            (None, None, None) => return Ok(None),
            (None, None, Some(path)) => {
                ConfiguredSource::SnapshotFile(required_path("THALAMUS_IDP_SNAPSHOT_FILE", path)?)
            }
            (id, secret, None) => ConfiguredSource::Keycloak {
                client_id: required_value("THALAMUS_IDP_KEYCLOAK_CLIENT_ID", id)?,
                client_secret: required_value("THALAMUS_IDP_KEYCLOAK_CLIENT_SECRET", secret)?,
            },
            _ => {
                return Err(
                    "IdP snapshot file and Keycloak credentials are mutually exclusive".into(),
                )
            }
        };

        let issuer = required_value("THALAMUS_OIDC_ISSUER", values.issuer)?;
        if !valid_issuer(&issuer) {
            return Err("invalid THALAMUS_OIDC_ISSUER for IdP synchronization".into());
        }
        // Unlike the server's default, explicit mapping is required when this
        // worker can remove roles. The operator must configure the same JSON on
        // both components, using {} when the shared mapping is intentionally empty.
        let mapping_json = required_value("THALAMUS_GROUP_MAPPING", values.mapping)?;
        let mapping: GroupMapping =
            serde_json::from_str(&mapping_json).map_err(|_| "invalid THALAMUS_GROUP_MAPPING")?;

        let (keycloak, snapshot_file) = match source {
            ConfiguredSource::SnapshotFile(path) => (None, Some(path)),
            ConfiguredSource::Keycloak {
                client_id,
                client_secret,
            } => {
                let ca_pem = match values.ca_file {
                    Some(path) => Some(read_ca(&required_path("THALAMUS_OIDC_CA_FILE", path)?)?),
                    None => None,
                };
                let collector = KeycloakCollector::new(KeycloakConfig {
                    issuer: issuer.clone(),
                    client_id,
                    client_secret,
                    ca_pem,
                })
                .map_err(|_| "invalid Keycloak IdP synchronization configuration")?;
                (Some(collector), None)
            }
        };

        Ok(Some(Self {
            issuer,
            mapping,
            keycloak,
            snapshot_file,
        }))
    }

    /// Obtain one complete current observation and apply it transactionally.
    /// The identity service enforces issuer, freshness, replay and idempotence.
    pub async fn reconcile(&self, identity: &WorkerIdentity) -> Result<()> {
        // Establish the fail-closed stale projection before contacting the IdP.
        // A failed or unavailable first observation must not leave keys active.
        identity
            .enable_idp_sync(&self.issuer)
            .await
            .map_err(|_| "IdP synchronization initialization failed")?;
        let snapshot = match (&self.keycloak, &self.snapshot_file) {
            (Some(collector), None) => collector
                .snapshot()
                .await
                .map_err(|_| "Keycloak IdP snapshot collection failed")?,
            (None, Some(path)) => read_snapshot(path).await?,
            _ => return Err("invalid IdP synchronization source".into()),
        };
        identity
            .sync_idp_snapshot(&self.issuer, &self.mapping, &snapshot)
            .await
            .map_err(|_| "IdP snapshot reconciliation failed")?;
        Ok(())
    }
}

fn required_value(name: &str, value: Option<OsString>) -> Result<String> {
    let value = value.ok_or_else(|| format!("{name} is required"))?;
    let value = value
        .into_string()
        .map_err(|_| format!("{name} must be valid UTF-8"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}

fn required_path(name: &str, value: OsString) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(PathBuf::from(value))
}

fn valid_issuer(issuer: &str) -> bool {
    if !issuer.starts_with("https://")
        || issuer["https://".len()..].starts_with('/')
        || issuer
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(issuer) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn read_ca(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open THALAMUS_OIDC_CA_FILE")?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read THALAMUS_OIDC_CA_FILE")?;
    if bytes.len() as u64 > MAX_CA_BYTES {
        return Err("THALAMUS_OIDC_CA_FILE is too large".into());
    }
    Ok(bytes)
}

async fn read_snapshot(path: &Path) -> Result<IdpSnapshot> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "cannot open THALAMUS_IDP_SNAPSHOT_FILE")?;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cannot read THALAMUS_IDP_SNAPSHOT_FILE")?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err("THALAMUS_IDP_SNAPSHOT_FILE is too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid THALAMUS_IDP_SNAPSHOT_FILE".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Environment {
        Environment {
            issuer: Some("https://idp.example/realms/example".into()),
            mapping: Some("{}".into()),
            ..Environment::default()
        }
    }

    fn path() -> PathBuf {
        env::temp_dir().join(format!("thalamus-idp-sync-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn opt_out_only_when_no_source_is_present() {
        assert!(matches!(
            IdentitySync::from_values(Environment::default()),
            Ok(None)
        ));
        assert!(matches!(IdentitySync::from_values(configured()), Ok(None)));
        let mut values = configured();
        values.client_id = Some("client".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.client_secret = Some("secret".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.client_id = Some("".into());
        values.client_secret = Some("secret".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.client_id = Some("client".into());
        values.client_secret = Some("  ".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("".into());
        assert!(IdentitySync::from_values(values).is_err());
    }

    #[test]
    fn requires_one_source_and_shared_authority() {
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        assert!(matches!(
            IdentitySync::from_values(values),
            Ok(Some(IdentitySync {
                keycloak: None,
                snapshot_file: Some(_),
                ..
            }))
        ));

        let mut values = configured();
        values.client_id = Some("client".into());
        values.client_secret = Some("secret".into());
        assert!(matches!(
            IdentitySync::from_values(values),
            Ok(Some(IdentitySync {
                keycloak: Some(_),
                snapshot_file: None,
                ..
            }))
        ));

        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.client_id = Some("client".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.client_secret = Some("secret".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.client_id = Some("client".into());
        values.client_secret = Some("secret".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.issuer = None;
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.mapping = None;
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.snapshot_file = Some("/operator/snapshot.json".into());
        values.mapping = Some("not JSON".into());
        assert!(IdentitySync::from_values(values).is_err());
        let mut values = configured();
        values.client_id = Some("client".into());
        values.client_secret = Some("secret".into());
        values.ca_file = Some("".into());
        assert!(IdentitySync::from_values(values).is_err());
    }

    #[test]
    fn issuer_must_be_a_safe_https_authority() {
        assert!(valid_issuer("https://idp.example/realms/example"));
        for invalid in [
            "http://idp.example/realms/example",
            "https://user:secret@idp.example/realm",
            "https://idp.example/realm?query=1",
            "https://idp.example/realm#fragment",
            "https://idp.example/realm\n",
            "https:///realm",
        ] {
            assert!(!valid_issuer(invalid));
        }
    }

    #[test]
    fn ca_file_is_bounded() {
        let file = path();
        std::fs::write(&file, vec![b'x'; MAX_CA_BYTES as usize]).unwrap();
        assert!(read_ca(&file).is_ok());
        std::fs::write(&file, vec![b'x'; MAX_CA_BYTES as usize + 1]).unwrap();
        assert!(read_ca(&file).is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(read_ca(&file).is_err());
    }

    #[tokio::test]
    async fn snapshot_file_is_bounded_and_must_be_complete_json() {
        let file = path();
        assert!(read_snapshot(&file).await.is_err());
        std::fs::write(&file, b"{\"issuer\":\"https://idp.example/realms/example\"").unwrap();
        assert!(read_snapshot(&file).await.is_err());
        std::fs::write(&file, b"{\"issuer\":\"https://idp.example/realms/example\",\"observed_at\":\"2026-10-28T00:00:00Z\"}").unwrap();
        assert!(read_snapshot(&file).await.is_err());
        let mut bytes = br#"{"issuer":"https://idp.example/realms/example","observed_at":"2026-10-28T00:00:00Z","subjects":[]}"#.to_vec();
        bytes.resize(MAX_SNAPSHOT_BYTES as usize, b' ');
        std::fs::write(&file, &bytes).unwrap();
        assert!(read_snapshot(&file).await.is_ok());
        bytes.push(b' ');
        std::fs::write(&file, &bytes).unwrap();
        assert!(read_snapshot(&file).await.is_err());
        std::fs::remove_file(&file).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_credentials_fail_instead_of_opt_out() {
        use std::os::unix::ffi::OsStringExt;
        let mut values = configured();
        values.client_id = Some(OsString::from_vec(vec![0xff]));
        values.client_secret = Some("secret".into());
        assert!(IdentitySync::from_values(values).is_err());
    }
}
