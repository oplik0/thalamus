use crate::{hex, Error, Result};
use bytes::Bytes;
use object_store::{path::Path, ObjectStore, PutMode, PutOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

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
