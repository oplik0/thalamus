use serde::{Deserialize, Serialize};

/// OPAQUE Registration Request (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationRequest {
    pub username: String,
    pub message: Vec<u8>, // RegistrationRequest message
}

/// OPAQUE Registration Response (Server -> Client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResponse {
    pub message: Vec<u8>, // RegistrationResponse message
}

/// OPAQUE Registration Record (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationRecord {
    pub username: String,
    pub message: Vec<u8>, // RegistrationRecord message
}

/// OPAQUE Login Request (Client -> Server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub message: Vec<u8>, // CredentialRequest message
}

/// OPAQUE Login Response (Server -> Client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub message: Vec<u8>,      // CredentialResponse message
    pub server_state: Vec<u8>, // Serialized ServerLogin state (to be echoed back)
}

/// OPAQUE Login Finalization (Client -> Server)
/// This is implicit in the protocol (client derives key), but we might want an explicit check
/// or just use the session key for subsequent requests.
/// For this implementation, we'll have the client send a "finish" message which is just the
/// second step of the handshake if we were doing a 3-step, but OPAQUE is 2-step for the core.
///
/// Actually, OPAQUE login is:
/// 1. Client sends CredentialRequest
/// 2. Server sends CredentialResponse
/// 3. Client derives session key and server authentication
/// 4. Server derives session key
///
/// To prove to the server that the client has completed the login, we usually do a
/// "session confirmation" step or just start using the session key.
///
/// For a REST API, we want to exchange this for a PASETO token.
/// So we need a step 3 where the client proves they derived the key, and the server issues a token.
///
/// We can use the "session key" to sign a challenge or just send a MAC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFinishRequest {
    pub username: String,
    pub login_request_message: Vec<u8>, // Original CredentialRequest (needed to reconstruct state if stateless)
    pub credential_response_message: Vec<u8>, // Original CredentialResponse (needed to reconstruct state if stateless)
    // Actually, we probably want to store state in Redis or similar, but for now let's assume
    // we can reconstruct or we use a "server login finish" message if the library supports it.
    //
    // opaque-ke doesn't have an explicit "server finish" message in the core protocol for the server to verify the client
    // *during* the handshake, but the session key is shared.
    //
    // Common pattern:
    // 1. Client -> Server: LoginStart (CredentialRequest)
    // 2. Server -> Client: LoginResponse (CredentialResponse)
    //    Server also computes session key but doesn't know if client succeeded yet.
    // 3. Client -> Server: LoginFinish (Proof of session key)
    //    Server verifies proof, issues PASETO.

    // We'll use a simple MAC over a known string using the session key as proof.
    pub session_proof: Vec<u8>,

    pub server_state: Vec<u8>, // Echoed back from LoginResponse
}
