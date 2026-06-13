use serde::{Deserialize, Serialize};

/// OPAQUE Registration Request (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationRequest {
    pub username: String,
    pub message: String, // base64-encoded RegistrationRequest message
}

/// OPAQUE Registration Response (Server -> Client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResponse {
    pub message: String, // base64-encoded RegistrationResponse message
}

/// OPAQUE Registration Record (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationRecord {
    pub username: String,
    pub message: String, // base64-encoded RegistrationUpload message
}

/// OPAQUE Login Request (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub message: String, // base64-encoded CredentialRequest message
}

/// OPAQUE Login Response (Server -> Client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub message: String,      // base64-encoded CredentialResponse message
    pub server_state: String, // base64-encoded serialized ServerLogin state
}

/// OPAQUE Login Finalization (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFinishRequest {
    pub username: String,
    pub finish_login_request: String, // base64-encoded CredentialFinalization
    pub server_state: String,         // base64-encoded serialized ServerLogin state (echoed back)
}
