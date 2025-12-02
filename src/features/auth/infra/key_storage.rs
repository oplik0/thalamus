use crate::bootstrap::AppState;
use argon2::password_hash::{SaltString, rand_core::OsRng};
/// API Key generation and validation
///
/// Uses database-stored random tokens with prefixes for easy identification
use argon2::{Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier};

pub fn store_key(full_key: &str, state: &AppState) -> Result<(), Box<dyn std::error::Error>> {
    // Hash the key for storage
    let salt = SaltString::generate(&mut OsRng);

    let key_hash = match Argon2::new_with_secret(
        // TODO: use config secret here
        b"some_secret_key_for_argon2",
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        // we have a random input so this is overkill anyway
        Params::new(1024, 2, 1, Some(64)).unwrap(),
        // unwrapping since the Params::new can only fail on invalid params
    ) {
        Ok(argon2) => argon2
            .hash_password(full_key.as_bytes(), &salt)
            .map_err(|e| format!("Failed to hash key: {}", e))?
            .to_string(),
        Err(e) => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to create Argon2 instance: {}", e),
            )));
        }
    };
    // Store the hashed key in the database
    return Ok(());
}
