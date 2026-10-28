use crate::{hex, Error, Result};
use bytes::Bytes;
use object_store::aws::{AmazonS3, AmazonS3Builder, S3ConditionalPut};
use object_store::{path::Path, ObjectStore, PutMode, PutOptions};
use object_store::{Certificate, ClientOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CA_BYTES: u64 = 1024 * 1024;

/// S3 configuration shared by the server and worker. Credentials are never
/// included in `Debug` output or in validation errors.
#[derive(Clone)]
pub struct S3Config {
    endpoint: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    root_certificates: Vec<Certificate>,
    ca_pem: Option<Vec<u8>>,
}

impl S3Config {
    /// The endpoint must be an HTTPS origin, as the same origin is used to
    /// generate run-scoped presigned upload URLs.
    pub fn new(
        endpoint: String,
        bucket: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
        ca_pem: Option<Vec<u8>>,
    ) -> Result<Self> {
        fn dns_label(value: &str) -> bool {
            !value.is_empty()
                && value.len() <= 63
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && value.as_bytes()[0].is_ascii_alphanumeric()
                && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        }
        let parsed =
            url::Url::parse(&endpoint).map_err(|_| Error::Invalid("invalid S3 endpoint".into()))?;
        if endpoint.len() > 1024
            || parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || bucket.len() < 3
            || !dns_label(&bucket)
            || !dns_label(&region)
        {
            return Err(Error::Invalid("invalid S3 configuration".into()));
        }
        crate::text(&access_key_id, "S3 access key", 4096)?;
        crate::text(&secret_access_key, "S3 secret key", 4096)?;
        let root_certificates = match ca_pem.as_deref() {
            Some(bytes) if !bytes.is_empty() && bytes.len() <= MAX_CA_BYTES as usize => {
                let certificates = Certificate::from_pem_bundle(bytes)
                    .map_err(|_| Error::Invalid("invalid S3 CA bundle".into()))?;
                if certificates.is_empty() {
                    return Err(Error::Invalid("invalid S3 CA bundle".into()));
                }
                certificates
            }
            Some(_) => return Err(Error::Invalid("invalid S3 CA bundle".into())),
            None => Vec::new(),
        };
        Ok(Self {
            endpoint,
            bucket,
            region,
            access_key_id,
            secret_access_key,
            root_certificates,
            ca_pem,
        })
    }

    /// Returns `None` only when no S3 configuration is present. A partial
    /// configuration fails closed, including a CA file without credentials.
    pub fn from_env() -> Result<Option<Self>> {
        const KEYS: [&str; 5] = [
            "THALAMUS_S3_ENDPOINT",
            "THALAMUS_S3_BUCKET",
            "THALAMUS_S3_REGION",
            "THALAMUS_S3_ACCESS_KEY_ID",
            "THALAMUS_S3_SECRET_ACCESS_KEY",
        ];
        if KEYS.iter().all(|key| std::env::var_os(key).is_none())
            && std::env::var_os("THALAMUS_S3_CA_FILE").is_none()
        {
            return Ok(None);
        }
        let get = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::Invalid(format!("{key} is required")))
        };
        let ca_pem = match std::env::var_os("THALAMUS_S3_CA_FILE") {
            Some(path) => {
                use std::io::Read;
                if path.is_empty() {
                    return Err(Error::Invalid("invalid S3 CA file".into()));
                }
                let file = std::fs::File::open(path)
                    .map_err(|_| Error::Invalid("cannot read S3 CA file".into()))?;
                let mut bytes = Vec::new();
                file.take(MAX_CA_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| Error::Invalid("cannot read S3 CA file".into()))?;
                Some(bytes)
            }
            None => None,
        };
        Self::new(
            get(KEYS[0])?,
            get(KEYS[1])?,
            get(KEYS[2])?,
            get(KEYS[3])?,
            get(KEYS[4])?,
            ca_pem,
        )
        .map(Some)
    }

    /// The validated PEM is copied into the owned per-run upload Secret so
    /// the runner can authenticate a private object-store endpoint.
    pub fn ca_pem(&self) -> Option<&[u8]> {
        self.ca_pem.as_deref()
    }

    /// Construct the same path-style S3 client and conditional-write behavior
    /// used by the worker; configured CA certificates extend the system roots.
    pub fn store(&self) -> Result<Arc<AmazonS3>> {
        let mut options = ClientOptions::new();
        for certificate in &self.root_certificates {
            options = options.with_root_certificate(certificate.clone());
        }
        let store = AmazonS3Builder::new()
            .with_endpoint(&self.endpoint)
            .with_bucket_name(&self.bucket)
            .with_region(&self.region)
            .with_access_key_id(&self.access_key_id)
            .with_secret_access_key(&self.secret_access_key)
            .with_client_options(options)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|_| Error::Invalid("cannot initialize S3 store".into()))?;
        Ok(Arc::new(store))
    }

    pub fn artifacts(&self) -> Result<Artifacts> {
        Ok(Artifacts::new(self.store()?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

impl ArtifactRef {
    pub(crate) fn validate_for(&self, run: Uuid) -> Result<()> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || self.path != format!("runs/{run}/{}", self.sha256)
            || self.size > MAX_ARTIFACT_BYTES as u64
        {
            return Err(Error::ArtifactIntegrity);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Artifacts {
    store: Arc<dyn ObjectStore>,
}

impl Artifacts {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Content-addressed writes use create-only preconditions. Existing objects
    /// are verified rather than overwritten, so retries are safe.
    pub async fn put(&self, run: Uuid, bytes: Bytes) -> Result<ArtifactRef> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(Error::Invalid("artifact exceeds 64 MiB".into()));
        }
        let digest = hex(&Sha256::digest(&bytes));
        let artifact = ArtifactRef {
            path: format!("runs/{run}/{digest}"),
            sha256: digest,
            size: bytes.len() as u64,
        };
        let path = Path::from(artifact.path.clone());
        match self
            .store
            .put_opts(
                &path,
                bytes.clone().into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {}
            Err(object_store::Error::AlreadyExists { .. }) => {
                self.read(run, &artifact).await?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(artifact)
    }

    /// Caller must first authorize access to the corresponding run's team.
    pub async fn read(&self, run: Uuid, artifact: &ArtifactRef) -> Result<Bytes> {
        artifact.validate_for(run)?;
        let bytes = self
            .store
            .get(&Path::from(artifact.path.clone()))
            .await?
            .bytes()
            .await?;
        if bytes.len() as u64 != artifact.size || hex(&Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(Error::ArtifactIntegrity);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3_config(endpoint: &str, ca: Option<&[u8]>) -> Result<S3Config> {
        S3Config::new(
            endpoint.into(),
            "run-results".into(),
            "us-east-1".into(),
            "access-key".into(),
            "secret-value".into(),
            ca.map(|bytes| bytes.to_vec()),
        )
    }

    #[test]
    fn s3_requires_https_origin_and_valid_optional_ca() {
        assert!(s3_config("https://objects.example", None).is_ok());
        for endpoint in [
            "http://objects.example",
            "https://user:password@objects.example",
            "https://objects.example/prefix",
            "https://objects.example/?credential=secret",
            "https://objects.example/#fragment",
        ] {
            assert!(s3_config(endpoint, None).is_err());
        }
        let error = s3_config("https://objects.example", Some(b"invalid PEM"))
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("secret-value"));
        assert!(s3_config("https://objects.example", Some(&[])).is_err());
        let config = S3Config::new(
            "https://objects.example".into(),
            "run-results".into(),
            "us-east-1".into(),
            "".into(),
            "secret-value".into(),
            None,
        );
        assert!(config.is_err());
    }
    #[tokio::test]
    async fn immutable_content_addressing_and_tenant_run_binding() {
        let store = Artifacts::new(Arc::new(object_store::memory::InMemory::new()));
        let id = Uuid::from_u128(42);
        let first = store.put(id, Bytes::from_static(b"result")).await.unwrap();
        assert_eq!(
            first,
            store.put(id, Bytes::from_static(b"result")).await.unwrap()
        );
        assert_eq!(
            store.read(id, &first).await.unwrap(),
            Bytes::from_static(b"result")
        );
        assert!(store.read(Uuid::from_u128(43), &first).await.is_err());
        let mut tampered = first;
        tampered.size += 1;
        assert!(matches!(
            store.read(id, &tampered).await,
            Err(Error::ArtifactIntegrity)
        ));
    }
}
