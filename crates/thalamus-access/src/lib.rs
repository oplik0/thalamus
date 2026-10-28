//! Read-only authorization from a trusted, published Git catalog revision.
//! Mutation authorization must instead hold the identity transaction guard.
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    process::Command,
};
use thalamus_identity::{IdentityService, VerifiedPrincipal};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceId {
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("catalog policy source unavailable, invalid, or stale")]
    Source,
    #[error("service access denied")]
    Forbidden,
    #[error("identity authorization failed: {0}")]
    Identity(#[from] thalamus_identity::Error),
}

/// Construct only from trusted server configuration, never from a request.
/// The revision must be the complete object ID of the published commit.
#[derive(Clone, Debug)]
pub struct PolicySource {
    repository: PathBuf,
    catalog: PathBuf,
    commit: String,
}

impl PolicySource {
    pub fn new(
        repository: impl Into<PathBuf>,
        catalog: impl Into<PathBuf>,
        commit: impl Into<String>,
    ) -> Self {
        Self {
            repository: repository.into(),
            catalog: catalog.into(),
            commit: commit.into(),
        }
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repository)
            .args(args)
            .output()
            .map_err(|_| Error::Source)?;
        if !output.status.success() {
            return Err(Error::Source);
        }
        Ok(output.stdout)
    }

    fn validate_source(&self) -> Result<(), Error> {
        if !matches!(self.commit.len(), 40 | 64)
            || !self
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.catalog.as_os_str().is_empty()
            || !self
                .catalog
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(Error::Source);
        }
        let head = self.git(&["rev-parse", "--verify", "HEAD^{commit}"])?;
        if std::str::from_utf8(&head)
            .map_err(|_| Error::Source)?
            .trim()
            != self.commit
        {
            return Err(Error::Source);
        }
        Ok(())
    }

    /// Read only committed blobs; uncommitted worktree changes cannot grant access.
    pub fn load(&self) -> Result<Policy, Error> {
        self.validate_source()?;
        let catalog = self.catalog.to_str().ok_or(Error::Source)?;
        // Reject rev/path metacharacters; a catalog path is not a Git revision expression.
        if catalog
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')))
        {
            return Err(Error::Source);
        }
        let tree = format!("{}:{}/services", self.commit, catalog);
        let entries = self.git(&["ls-tree", "-z", &tree])?;
        let mut services = BTreeMap::new();
        let mut committed_services = Vec::new();
        let mut names = BTreeSet::new();
        for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let separator = entry
                .iter()
                .position(|b| *b == b'\t')
                .ok_or(Error::Source)?;
            let (metadata, name) = (&entry[..separator], &entry[separator + 1..]);
            let name = std::str::from_utf8(name).map_err(|_| Error::Source)?;
            if !matches!(
                Path::new(name).extension().and_then(|e| e.to_str()),
                Some("yaml" | "yml")
            ) {
                continue;
            }
            let metadata = std::str::from_utf8(metadata).map_err(|_| Error::Source)?;
            let mut fields = metadata.split(' ');
            let mode = fields.next().ok_or(Error::Source)?;
            let kind = fields.next().ok_or(Error::Source)?;
            let object = fields.next().ok_or(Error::Source)?;
            if fields.next().is_some() || !matches!(mode, "100644" | "100755") || kind != "blob" {
                return Err(Error::Source);
            }
            let bytes = self.git(&["cat-file", "blob", object])?;
            let raw: RawService = serde_yaml::from_slice(&bytes).map_err(|_| Error::Source)?;
            if raw.name.is_empty()
                || raw.namespace.is_empty()
                || raw.name.trim() != raw.name
                || raw.namespace.trim() != raw.namespace
                || !names.insert(raw.name.clone())
            {
                return Err(Error::Source);
            }
            let mut teams = BTreeSet::new();
            for team in raw.access.teams {
                if team.is_nil() || !teams.insert(team) {
                    return Err(Error::Source);
                }
            }
            let mut deployments = BTreeSet::new();
            for deployment in raw.deployments {
                if deployment.name.is_empty()
                    || deployment.name.trim() != deployment.name
                    || !deployments.insert(deployment.name)
                {
                    return Err(Error::Source);
                }
            }
            let id = ServiceId {
                name: raw.name,
                namespace: raw.namespace,
            };
            if services
                .insert(id, ServicePolicy { teams, deployments })
                .is_some()
            {
                return Err(Error::Source);
            }
            committed_services.push((name.to_owned(), bytes));
        }
        self.validate_source()?;
        Ok(Policy {
            source: self.clone(),
            services,
            committed_services,
        })
    }
}

#[derive(Deserialize)]
struct RawService {
    name: String,
    namespace: String,
    access: RawAccess,
    deployments: Vec<RawDeployment>,
}
#[derive(Deserialize)]
struct RawDeployment {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccess {
    teams: Vec<Uuid>,
}

/// A snapshot of the published policy. Each authorization rechecks its revision.
struct ServicePolicy {
    teams: BTreeSet<Uuid>,
    deployments: BTreeSet<String>,
}
pub struct Policy {
    source: PolicySource,
    services: BTreeMap<ServiceId, ServicePolicy>,
    committed_services: Vec<(String, Vec<u8>)>,
}

fn copy_principal<'a>(principal: &VerifiedPrincipal<'a>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(session) => VerifiedPrincipal::Session(session),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

impl Policy {
    /// Return the exact service blobs used to construct this policy, after
    /// checking that its trusted source is still at the configured revision.
    pub fn committed_services(&self) -> Result<&[(String, Vec<u8>)], Error> {
        self.source.validate_source()?;
        Ok(&self.committed_services)
    }

    /// Read a direct regular file from the same committed catalog snapshot.
    pub fn committed_catalog_blob(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.source.validate_source()?;
        if name.is_empty()
            || name == "."
            || name == ".."
            || name
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
        {
            return Err(Error::Source);
        }
        let catalog = self.source.catalog.to_str().ok_or(Error::Source)?;
        let tree = format!("{}:{}", self.source.commit, catalog);
        let entry = self.source.git(&["ls-tree", "-z", &tree, "--", name])?;
        let entry = entry.strip_suffix(&[0]).ok_or(Error::Source)?;
        let separator = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or(Error::Source)?;
        if &entry[separator + 1..] != name.as_bytes() {
            return Err(Error::Source);
        }
        let metadata = std::str::from_utf8(&entry[..separator]).map_err(|_| Error::Source)?;
        let mut fields = metadata.split(' ');
        let mode = fields.next().ok_or(Error::Source)?;
        let kind = fields.next().ok_or(Error::Source)?;
        let object = fields.next().ok_or(Error::Source)?;
        if fields.next().is_some() || !matches!(mode, "100644" | "100755") || kind != "blob" {
            return Err(Error::Source);
        }
        let contents = self.source.git(&["cat-file", "blob", object])?;
        self.source.validate_source()?;
        Ok(contents)
    }

    /// Resolve a service name to its exact committed namespace and name.
    pub fn service_id(&self, name: &str) -> Result<Option<ServiceId>, Error> {
        self.source.validate_source()?;
        Ok(self.services.keys().find(|id| id.name == name).cloned())
    }

    /// Check the exact deployment in the same committed service as the policy.
    pub fn includes_deployment(
        &self,
        service: &ServiceId,
        deployment: &str,
    ) -> Result<bool, Error> {
        self.source.validate_source()?;
        Ok(self
            .services
            .get(service)
            .ok_or(Error::Forbidden)?
            .deployments
            .contains(deployment))
    }

    async fn permissions(
        &self,
        identity: &IdentityService,
        principal: VerifiedPrincipal<'_>,
        service: &ServiceId,
    ) -> Result<(bool, Vec<Uuid>), Error> {
        self.source.validate_source()?;
        let allowed = &self.services.get(service).ok_or(Error::Forbidden)?.teams;
        let (admin, teams) = match identity
            .begin_platform_admin(copy_principal(&principal))
            .await
        {
            Ok(guard) => {
                guard.commit().await?;
                (true, allowed.iter().copied().collect())
            }
            Err(thalamus_identity::Error::Forbidden) => {
                let memberships = identity.list_teams_principal(principal).await?;
                (
                    false,
                    memberships
                        .into_iter()
                        .filter_map(|team| allowed.contains(&team.id).then_some(team.id))
                        .collect(),
                )
            }
            Err(error) => return Err(error.into()),
        };
        self.source.validate_source()?;
        Ok((admin, teams))
    }

    pub async fn authorize(
        &self,
        identity: &IdentityService,
        principal: VerifiedPrincipal<'_>,
        service: &ServiceId,
    ) -> Result<(), Error> {
        let (admin, teams) = self.permissions(identity, principal, service).await?;
        if admin || !teams.is_empty() {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    /// Return only allowed team IDs currently accessible to the principal.
    /// Callers requiring one tenant must reject an empty or ambiguous result.
    pub async fn authorized_teams(
        &self,
        identity: &IdentityService,
        principal: VerifiedPrincipal<'_>,
        service: &ServiceId,
    ) -> Result<Vec<Uuid>, Error> {
        let (_, teams) = self.permissions(identity, principal, service).await?;
        if teams.is_empty() {
            Err(Error::Forbidden)
        } else {
            Ok(teams)
        }
    }
}
