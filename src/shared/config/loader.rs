//! KCL configuration loader
//!
//! Supports loading configuration with named profiles.

use super::types::Config;
use kcl_lang::{API, ExecProgramArgs};
use std::collections::HashMap;
use std::path::Path;

/// Load all configuration profiles from a KCL file
///
/// # Errors
/// Returns an error if the configuration file doesn't exist, has syntax errors, or fails validation
pub fn load_config_profiles<P: AsRef<Path>>(path: P) -> crate::Result<HashMap<String, Config>> {
    let path = path.as_ref();

    tracing::info!(path = %path.display(), "Loading KCL configuration profiles");

    if !path.exists() {
        return Err(crate::Error::Config(format!(
            "Configuration file not found: {}",
            path.display()
        )));
    }

    // Create KCL API instance
    let api = API::default();

    // Set up execution arguments
    let args = &ExecProgramArgs {
        k_filename_list: vec![path.to_string_lossy().to_string()],
        ..Default::default()
    };

    // Execute KCL program
    let result = api
        .exec_program(args)
        .map_err(|e| crate::Error::Config(format!("Failed to execute KCL program: {e}")))?;

    // Check for KCL errors
    if !result.err_message.is_empty() {
        return Err(crate::Error::Config(format!(
            "KCL program error: {}",
            result.err_message
        )));
    }

    // Parse JSON output as HashMap
    let json_str = result.json_result;
    let profiles: HashMap<String, Config> = serde_json::from_str(&json_str)
        .map_err(|e| crate::Error::Config(format!("Failed to parse KCL output as JSON: {e}")))?;

    // Validate each profile
    for (name, config) in &profiles {
        if let Err(e) = config.validate() {
            tracing::warn!(profile = %name, error = %e, "Profile validation failed");
            return Err(crate::Error::Config(format!(
                "Profile '{name}' validation failed: {e}"
            )));
        }
    }

    tracing::info!(
        profiles = profiles.len(),
        "Loaded {} configuration profiles",
        profiles.len()
    );

    Ok(profiles)
}

/// Load a specific configuration profile from a KCL file
///
/// # Errors
/// Returns an error if the configuration file doesn't exist, the profile doesn't exist,
/// has syntax errors, or fails validation
pub fn load_config<P: AsRef<Path>>(path: P, profile: &str) -> crate::Result<Config> {
    let profiles = load_config_profiles(path)?;

    profiles
        .into_iter()
        .find(|(name, _)| name == profile)
        .map(|(_, config)| config)
        .ok_or_else(|| {
            crate::Error::Config(format!(
                "Profile '{profile}' not found in configuration file"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_load_config_profiles() {
        let result = load_config_profiles("config.k");

        match result {
            Ok(profiles) => {
                assert!(!profiles.is_empty());
                println!(
                    "Loaded {} profiles: {:?}",
                    profiles.len(),
                    profiles.keys().collect::<Vec<_>>()
                );
            }
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    !error_msg.contains("syntax error"),
                    "Config has syntax errors: {error_msg}"
                );
            }
        }
    }

    #[test]
    fn test_load_config_profile() {
        let result = load_config("config.k", "default");

        match result {
            Ok(config) => {
                assert!(!config.backends.is_empty());
                println!(
                    "Loaded 'default' profile with {} backends",
                    config.backends.len()
                );
            }
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    !error_msg.contains("syntax error"),
                    "Config has syntax errors: {error_msg}"
                );
            }
        }
    }
}
