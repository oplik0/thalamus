//! Authorization domain traits
//!
//! Defines the interface for authorization enforcement.
//! This allows swapping Casbin for another authorization system if needed.

use crate::error::Result;
use async_trait::async_trait;

/// Authorization request context
///
/// Represents a request to check authorization for.
/// - `subject`: The user or service making the request (e.g., username, user_id)
/// - `domain`: The team/organization context (e.g., team_id)
/// - `object`: The resource being accessed (e.g., API path)
/// - `action`: The action being performed (e.g., HTTP method)
#[derive(Debug, Clone)]
pub struct AuthRequest {
    pub subject: String,
    pub domain: String,
    pub object: String,
    pub action: String,
}

impl AuthRequest {
    /// Create a new authorization request
    pub fn new(
        subject: impl Into<String>,
        domain: impl Into<String>,
        object: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            domain: domain.into(),
            object: object.into(),
            action: action.into(),
        }
    }
}

/// Authorization enforcer trait
///
/// This trait abstracts the authorization enforcement mechanism.
/// Implementations should be thread-safe and cheap to clone (e.g., use Arc internally).
#[async_trait]
pub trait Authorizer: Send + Sync + Clone {
    /// Check if a request is authorized
    ///
    /// # Arguments
    /// * `request` - The authorization request to evaluate
    ///
    /// # Returns
    /// `true` if the request is authorized, `false` otherwise
    async fn is_authorized(&self, request: &AuthRequest) -> Result<bool>;

    /// Enforce authorization, returning an error if not authorized
    ///
    /// # Arguments
    /// * `request` - The authorization request to evaluate
    ///
    /// # Errors
    /// Returns an Authorization error if the request is not allowed
    async fn enforce(&self, request: &AuthRequest) -> Result<()> {
        if self.is_authorized(request).await? {
            Ok(())
        } else {
            Err(crate::Error::Authorization(format!(
                "Access denied: {} cannot {} {} in domain {}",
                request.subject, request.action, request.object, request.domain
            )))
        }
    }
}

/// Policy management trait
///
/// Provides methods to manage Casbin policies and roles.
/// This is separate from the enforcer to allow read-only enforcers
/// in some contexts.
#[async_trait]
pub trait PolicyManager: Send + Sync {
    /// Add a policy rule
    ///
    /// # Arguments
    /// * `subject` - The subject (role or user)
    /// * `domain` - The domain (team) or "*" for all domains
    /// * `object` - The resource (e.g., API path) or "*" for all
    /// * `action` - The action (e.g., HTTP method) or "*" for all
    async fn add_policy(
        &self,
        subject: &str,
        domain: &str,
        object: &str,
        action: &str,
    ) -> Result<bool>;

    /// Remove a policy rule
    async fn remove_policy(
        &self,
        subject: &str,
        domain: &str,
        object: &str,
        action: &str,
    ) -> Result<bool>;

    /// Add a role assignment (grouping policy)
    ///
    /// # Arguments
    /// * `user` - The user to assign the role to
    /// * `role` - The role to assign
    /// * `domain` - The domain (team) for the role assignment
    async fn add_role(&self, user: &str, role: &str, domain: &str) -> Result<bool>;

    /// Remove a role assignment
    async fn remove_role(&self, user: &str, role: &str, domain: &str) -> Result<bool>;

    /// Get all roles for a user in a domain
    async fn get_roles(&self, user: &str, domain: &str) -> Result<Vec<String>>;

    /// Get all policies
    async fn get_policies(&self) -> Result<Vec<Vec<String>>>;

    /// Get all policies for a subject in a domain
    async fn get_policies_for_subject(
        &self,
        subject: &str,
        domain: &str,
    ) -> Result<Vec<Vec<String>>>;
}
