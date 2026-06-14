use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::opaque::{
    LoginFinishRequest, LoginRequest, LoginResponse, RegistrationRecord, RegistrationRequest,
    RegistrationResponse,
};
use crate::features::auth::domain::token::TokenClaims;
use crate::features::auth::infra::token_service::create_token;
use base64::Engine;
use opaque_ke::generic_array;
use opaque_ke::{
    CipherSuite, CredentialFinalization, CredentialRequest,
    RegistrationRequest as OpaqueRegistrationRequest, RegistrationUpload, ServerLogin,
    ServerLoginParameters, ServerRegistration, ServerSetup,
};
use rand_08::SeedableRng;
use rand_08::rngs::OsRng;
use sha2::Sha512;

/// OPAQUE key-stretching parameters matching `@serenity-kit/opaque`'s
/// `memory-constrained` default.
const KSF_MEMORY_KIB: u32 = 65_536;
const KSF_ITERATIONS: u32 = 3;
const KSF_PARALLELISM: u32 = 4;

/// Base64 engine used by `@serenity-kit/opaque`.
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Custom key-stretching function matching the JS OPAQUE bindings.
#[derive(Debug)]
pub struct CustomKsf {
    argon: argon2::Argon2<'static>,
}

impl Default for CustomKsf {
    fn default() -> Self {
        let params = argon2::Params::new(KSF_MEMORY_KIB, KSF_ITERATIONS, KSF_PARALLELISM, None)
            .expect("valid OPAQUE KSF parameters");
        let argon =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        Self { argon }
    }
}

impl opaque_ke::ksf::Ksf for CustomKsf {
    fn hash<L: generic_array::ArrayLength<u8>>(
        &self,
        input: generic_array::GenericArray<u8, L>,
    ) -> std::result::Result<generic_array::GenericArray<u8, L>, opaque_ke::errors::InternalError>
    {
        let mut output = generic_array::GenericArray::default();
        self.argon
            .hash_password_into(&input, &[0; argon2::RECOMMENDED_SALT_LEN], &mut output)
            .map_err(|_| opaque_ke::errors::InternalError::KsfError)?;
        Ok(output)
    }
}

// Define the OPAQUE cipher suite
// We use Ristretto255 as the group and SHA-512 as the hash function, with a
// custom Argon2id KSF that matches `@serenity-kit/opaque`.
#[derive(Debug)]
pub struct ThalamusCipherSuite;

impl CipherSuite for ThalamusCipherSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDh<opaque_ke::Ristretto255, Sha512>;
    type Ksf = CustomKsf;
}

fn encode_base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

fn decode_base64(s: &str) -> Result<Vec<u8>> {
    // Accept either URL-safe no-pad (JS bindings) or standard base64.
    BASE64.decode(s).or_else(|_| {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|e| Error::Authentication(format!("Invalid base64: {e}")))
    })
}

/// Handle OPAQUE registration start
pub async fn registration_start(
    request: RegistrationRequest,
    state: &AppState,
) -> Result<RegistrationResponse> {
    let server_setup = get_server_setup(state)?;

    let request_bytes = decode_base64(&request.message)?;
    let opaque_request =
        OpaqueRegistrationRequest::<ThalamusCipherSuite>::deserialize(&request_bytes)
            .map_err(|e| Error::Authentication(format!("Invalid registration request: {e}")))?;

    let registration_start = ServerRegistration::<ThalamusCipherSuite>::start(
        &server_setup,
        opaque_request,
        request.username.as_bytes(),
    )
    .map_err(|e| Error::Authentication(format!("OPAQUE registration start failed: {e}")))?;

    Ok(RegistrationResponse {
        message: encode_base64(&registration_start.message.serialize()),
    })
}

/// Handle OPAQUE registration finish
pub async fn registration_finish(request: RegistrationRecord, state: &AppState) -> Result<()> {
    let registration_bytes = finish_registration_upload(&request.message)?;

    // Store in database; user must already exist.
    let user_exists = sqlx::query!("SELECT id FROM users WHERE username = $1", request.username)
        .fetch_optional(&state.db_pool)
        .await?
        .is_some();

    if user_exists {
        sqlx::query!(
            "UPDATE users SET opaque_registration = $1 WHERE username = $2",
            registration_bytes,
            request.username
        )
        .execute(&state.db_pool)
        .await?;
    } else {
        return Err(Error::InvalidInput(format!(
            "User {} does not exist",
            request.username
        )));
    }

    Ok(())
}

/// Convert a client OPAQUE registration upload into the bytes stored for login.
pub fn finish_registration_upload(message: &str) -> Result<Vec<u8>> {
    let upload_bytes = decode_base64(message)?;
    let opaque_upload = RegistrationUpload::<ThalamusCipherSuite>::deserialize(&upload_bytes)
        .map_err(|e| Error::Authentication(format!("Invalid registration upload: {e}")))?;

    let password_file = ServerRegistration::<ThalamusCipherSuite>::finish(opaque_upload);
    Ok(password_file.serialize().to_vec())
}

/// Handle OPAQUE login start
pub async fn login_start(request: LoginRequest, state: &AppState) -> Result<LoginResponse> {
    let server_setup = get_server_setup(state)?;

    // Fetch user's registration record
    let row = sqlx::query!(
        "SELECT opaque_registration FROM users WHERE username = $1",
        request.username
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let registration_bytes = match row.and_then(|r| r.opaque_registration) {
        Some(bytes) => bytes,
        None => {
            return Err(Error::Authentication(
                "User not found or not registered".to_string(),
            ));
        }
    };

    let password_file = ServerRegistration::<ThalamusCipherSuite>::deserialize(&registration_bytes)
        .map_err(|e| Error::Internal(format!("Failed to deserialize registration: {e}")))?;

    let credential_request_bytes = decode_base64(&request.message)?;
    let credential_request =
        CredentialRequest::<ThalamusCipherSuite>::deserialize(&credential_request_bytes)
            .map_err(|e| Error::Authentication(format!("Invalid credential request: {e}")))?;

    let mut rng = OsRng;
    let login_start = ServerLogin::start(
        &mut rng,
        &server_setup,
        Some(password_file),
        credential_request,
        request.username.as_bytes(),
        ServerLoginParameters::default(),
    )
    .map_err(|e| Error::Authentication(format!("OPAQUE login start failed: {e}")))?;

    Ok(LoginResponse {
        message: encode_base64(&login_start.message.serialize()),
        server_state: encode_base64(&login_start.state.serialize()),
    })
}

/// Handle OPAQUE login finish
pub async fn login_finish(request: LoginFinishRequest, state: &AppState) -> Result<String> {
    let server_state_bytes = decode_base64(&request.server_state)?;
    let server_state: ServerLogin<ThalamusCipherSuite> =
        ServerLogin::deserialize(&server_state_bytes)
            .map_err(|e| Error::Authentication(format!("Invalid server state: {e}")))?;

    let credential_finalization_bytes = decode_base64(&request.finish_login_request)?;
    let credential_finalization =
        CredentialFinalization::<ThalamusCipherSuite>::deserialize(&credential_finalization_bytes)
            .map_err(|e| Error::Authentication(format!("Invalid credential finalization: {e}")))?;

    let _session_key = ServerLogin::finish(
        server_state,
        credential_finalization,
        ServerLoginParameters::default(),
    )
    .map_err(|e| Error::Authentication(format!("OPAQUE login finish failed: {e}")))?;

    // Get user ID
    let user = sqlx::query!(
        "SELECT id, is_service_account FROM users WHERE username = $1",
        request.username
    )
    .fetch_optional(&state.db_pool)
    .await?
    .ok_or_else(|| Error::Authentication("User not found".to_string()))?;

    // Get team ID (default team for now)
    let team = sqlx::query!(
        r#"
        SELECT team_id, role
        FROM team_memberships
        WHERE user_id = $1
        ORDER BY created_at ASC
        LIMIT 1
        "#,
        user.id
    )
    .fetch_optional(&state.db_pool)
    .await?
    .ok_or_else(|| Error::Authentication("User has no team memberships".to_string()))?;

    // Derive scopes from role, mirroring OAuth provisioning.
    let scopes = derive_scopes(&team.role);

    // Create PASETO token
    let claims = TokenClaims::new(
        user.id,
        team.team_id,
        Some(vec![team.role]),
        Some(scopes),
        3600 * 24, // 24 hours
    );

    create_token(&claims, state)
}

fn derive_scopes(role: &str) -> Vec<String> {
    if role == "admin" {
        vec![
            "api_keys:read".to_string(),
            "api_keys:create".to_string(),
            "api_keys:revoke".to_string(),
            "api_keys:rotate".to_string(),
            "signing_keys:read".to_string(),
            "signing_keys:create".to_string(),
            "signing_keys:revoke".to_string(),
            "tokens:read".to_string(),
            "tokens:create".to_string(),
            "tokens:revoke".to_string(),
            "oauth:link".to_string(),
            "oauth:unlink".to_string(),
            "admin".to_string(),
        ]
    } else {
        vec!["api_keys:read".to_string(), "signing_keys:read".to_string()]
    }
}

// Helper to get server setup from config
pub fn get_server_setup(state: &AppState) -> Result<ServerSetup<ThalamusCipherSuite>> {
    let config = state.config.as_ref();

    if config.security.opaque_server_setup == "dev"
        || config.security.opaque_server_setup.is_empty()
    {
        // Deterministic setup for dev based on api_key_secret.
        let seed = config.security.api_key_secret.as_bytes();
        let mut seed_bytes = [0u8; 32];
        for (i, b) in seed.iter().enumerate().take(32) {
            seed_bytes[i] = *b;
        }

        let mut rng = rand_08::rngs::StdRng::from_seed(seed_bytes);
        return Ok(ServerSetup::new(&mut rng));
    }

    let setup_bytes = decode_base64(&config.security.opaque_server_setup)?;

    // First, try interpreting the config value as a serialized ServerSetup
    // (the format produced by `opaque.server.createSetup()`).
    if let Ok(setup) = ServerSetup::<ThalamusCipherSuite>::deserialize(&setup_bytes) {
        return Ok(setup);
    }

    // Fall back to treating it as a 32-byte seed.
    let mut seed_bytes = [0u8; 32];
    let len = setup_bytes.len().min(32);
    seed_bytes[..len].copy_from_slice(&setup_bytes[..len]);

    let mut rng = rand_08::rngs::StdRng::from_seed(seed_bytes);
    Ok(ServerSetup::new(&mut rng))
}
