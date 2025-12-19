//! Casbin authorization infrastructure
//!
//! Implements the authorization domain traits using Casbin with SQLx adapter.

use crate::error::{Error, Result};
use crate::features::authorization::domain::{AuthRequest, Authorizer, PolicyManager};
use async_trait::async_trait;
use casbin::{CoreApi, DefaultModel, Enforcer, MgmtApi, RbacApi};
use sqlx::PgPool;
use sqlx_adapter::SqlxAdapter;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Casbin-based authorizer
///
/// This struct wraps a Casbin enforcer and implements the Authorizer trait.
/// It is thread-safe and cheap to clone (uses Arc internally).
#[derive(Clone)]
pub struct CasbinAuthorizer {
    enforcer: Arc<RwLock<Enforcer>>,
}

impl std::fmt::Debug for CasbinAuthorizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CasbinAuthorizer")
            .field("enforcer", &"<Enforcer>")
            .finish()
    }
}

impl CasbinAuthorizer {
    /// Create a new Casbin authorizer from a database pool
    ///
    /// # Arguments
    /// * `pool` - PostgreSQL connection pool for loading policies
    ///
    /// # Errors
    /// Returns an error if the Casbin model cannot be loaded or the adapter fails
    pub async fn new(pool: PgPool) -> Result<Self> {
        // Load the Casbin model from the configuration file
        let model = DefaultModel::from_file("casbin_model.conf")
            .await
            .map_err(|e| Error::Config(format!("Failed to load Casbin model: {}", e)))?;

        // Create the SQLx adapter using the existing pool
        let adapter = SqlxAdapter::new_with_pool(pool)
            .await
            .map_err(|e| Error::Internal(format!("Failed to create Casbin adapter: {}", e)))?;

        // Create the enforcer using CoreApi::new
        let enforcer = CoreApi::new(model, adapter)
            .await
            .map_err(|e| Error::Internal(format!("Failed to create Casbin enforcer: {}", e)))?;

        tracing::info!("Casbin authorizer initialized successfully");

        Ok(Self {
            enforcer: Arc::new(RwLock::new(enforcer)),
        })
    }

    /// Create a new Casbin authorizer with a custom model file path
    ///
    /// # Arguments
    /// * `pool` - PostgreSQL connection pool for loading policies
    /// * `model_path` - Path to the Casbin model configuration file
    pub async fn with_model(pool: PgPool, model_path: &str) -> Result<Self> {
        let model = DefaultModel::from_file(model_path).await.map_err(|e| {
            Error::Config(format!(
                "Failed to load Casbin model from {}: {}",
                model_path, e
            ))
        })?;

        let adapter = SqlxAdapter::new_with_pool(pool)
            .await
            .map_err(|e| Error::Internal(format!("Failed to create Casbin adapter: {}", e)))?;

        let enforcer = CoreApi::new(model, adapter)
            .await
            .map_err(|e| Error::Internal(format!("Failed to create Casbin enforcer: {}", e)))?;

        tracing::info!("Casbin authorizer initialized with model: {}", model_path);

        Ok(Self {
            enforcer: Arc::new(RwLock::new(enforcer)),
        })
    }

    /// Reload policies from the database
    ///
    /// Call this after making changes to policies to ensure
    /// the enforcer has the latest data.
    pub async fn reload_policies(&self) -> Result<()> {
        let mut enforcer = self.enforcer.write().await;
        CoreApi::load_policy(&mut *enforcer)
            .await
            .map_err(|e| Error::Internal(format!("Failed to reload policies: {}", e)))?;
        tracing::debug!("Casbin policies reloaded");
        Ok(())
    }
}

#[async_trait]
impl Authorizer for CasbinAuthorizer {
    async fn is_authorized(&self, request: &AuthRequest) -> Result<bool> {
        let enforcer = self.enforcer.read().await;
        let allowed = CoreApi::enforce(
            &*enforcer,
            (
                &request.subject,
                &request.domain,
                &request.object,
                &request.action,
            ),
        )
        .map_err(|e| Error::Authorization(format!("Authorization check failed: {}", e)))?;
        Ok(allowed)
    }
}

#[async_trait]
impl PolicyManager for CasbinAuthorizer {
    async fn add_policy(
        &self,
        subject: &str,
        domain: &str,
        object: &str,
        action: &str,
    ) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        let added = MgmtApi::add_policy(
            &mut *enforcer,
            vec![
                subject.to_string(),
                domain.to_string(),
                object.to_string(),
                action.to_string(),
            ],
        )
        .await
        .map_err(|e| Error::Internal(format!("Failed to add policy: {}", e)))?;
        Ok(added)
    }

    async fn remove_policy(
        &self,
        subject: &str,
        domain: &str,
        object: &str,
        action: &str,
    ) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        let removed = MgmtApi::remove_policy(
            &mut *enforcer,
            vec![
                subject.to_string(),
                domain.to_string(),
                object.to_string(),
                action.to_string(),
            ],
        )
        .await
        .map_err(|e| Error::Internal(format!("Failed to remove policy: {}", e)))?;
        Ok(removed)
    }

    async fn add_role(&self, user: &str, role: &str, domain: &str) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        let added = MgmtApi::add_grouping_policy(
            &mut *enforcer,
            vec![user.to_string(), role.to_string(), domain.to_string()],
        )
        .await
        .map_err(|e| Error::Internal(format!("Failed to add role: {}", e)))?;
        Ok(added)
    }

    async fn remove_role(&self, user: &str, role: &str, domain: &str) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        let removed = MgmtApi::remove_grouping_policy(
            &mut *enforcer,
            vec![user.to_string(), role.to_string(), domain.to_string()],
        )
        .await
        .map_err(|e| Error::Internal(format!("Failed to remove role: {}", e)))?;
        Ok(removed)
    }

    async fn get_roles(&self, user: &str, domain: &str) -> Result<Vec<String>> {
        let enforcer = self.enforcer.read().await;
        // get_roles_for_user is from RbacApi trait
        let roles = RbacApi::get_roles_for_user(&*enforcer, user, Some(domain));
        Ok(roles)
    }

    async fn get_policies(&self) -> Result<Vec<Vec<String>>> {
        let enforcer = self.enforcer.read().await;
        let policies = enforcer.get_policy();
        Ok(policies)
    }

    async fn get_policies_for_subject(
        &self,
        subject: &str,
        domain: &str,
    ) -> Result<Vec<Vec<String>>> {
        let enforcer = self.enforcer.read().await;
        let policies =
            enforcer.get_filtered_policy(0, vec![subject.to_string(), domain.to_string()]);
        Ok(policies)
    }
}

/// Factory for creating Casbin authorizers
///
/// This allows lazy initialization of the authorizer.
#[derive(Debug)]
pub struct CasbinAuthorizerFactory;

impl CasbinAuthorizerFactory {
    /// Create a new Casbin authorizer from a database pool
    pub async fn create(pool: PgPool) -> Result<CasbinAuthorizer> {
        CasbinAuthorizer::new(pool).await
    }
}

#[cfg(test)]
mod tests {

    // Note: These tests require a running PostgreSQL database with Casbin schema
    // They are marked as ignored by default and should be run with:
    // cargo test --features integration-tests -- --ignored

    #[tokio::test]
    #[ignore]
    async fn test_casbin_authorizer_new() {
        // This test would require a test database setup
        // Skipping for now as it requires database infrastructure
    }
}
