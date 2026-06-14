pub mod http_signature;
pub mod key_rotation;
pub mod key_storage;
pub mod oauth_providers;
pub mod oauth_service;
pub mod oauth_state;
pub mod opaque_service;
pub mod signing_key_management;
pub mod tasks;
pub mod token_revocation;
pub mod token_service;

pub use http_signature::{HttpSignatureVerifier, VerifiedSignature};
pub use key_rotation::{
    GracePeriodStatus, KeyInGracePeriod, KeyRotationResult, RotationHistoryEntry,
    check_grace_period_status, get_rotation_history, list_keys_in_grace_period,
    revoke_expired_grace_period_keys, rotate_key_immediate, rotate_key_with_grace_period,
};
pub use key_storage::{list_team_keys, list_user_keys, revoke_key, store_key, validate_key};
pub use oauth_providers::{GitHubEnterpriseProvider, GitHubOAuthProvider};
pub use oauth_service::{OAuthAuthResponse, OAuthInitiateResponse, OAuthService, ProviderInfo};
pub use oauth_state::{InMemoryOAuthStateStore, OAuthStateStore, create_oauth_flow_state};
pub use opaque_service::{
    get_server_setup, login_finish, login_start, registration_finish, registration_start,
};
pub use signing_key_management::{
    GeneratedKeyPair, SignatureAlgorithm, SigningKey, create_signing_key, get_signing_key,
    get_signing_key_by_fingerprint, list_user_signing_keys, revoke_signing_key,
};
pub use tasks::{UpdateKeyUsageTask, init_task_db_pool};
pub use token_revocation::{
    RefreshTokenInfo, cleanup_expired_refresh_tokens, cleanup_expired_revocations,
    create_refresh_token, detect_refresh_token_reuse, is_token_revoked, list_user_refresh_tokens,
    revoke_all_user_tokens, revoke_refresh_token, revoke_refresh_token_family, revoke_token,
    rotate_refresh_token, validate_refresh_token,
};
pub use token_service::{create_token, validate_token};
