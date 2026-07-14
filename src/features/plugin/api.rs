//! Plugin admin API endpoints
//!
//! Provides HTTP endpoints for managing plugins at runtime.
//! All endpoints require admin authentication.

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::middleware::{ApiKeyAuth, require_scope};
use crate::shared::config::types::{PluginManifest, PluginType};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// DTOs
// ─────────────────────────────────────────────────────────────────────────────

/// Response wrapper for listing plugins
#[derive(Debug, Serialize)]
pub struct PluginListResponse {
    pub plugins: Vec<PluginInfoDto>,
}

/// Single plugin info representation
#[derive(Debug, Serialize)]
pub struct PluginInfoDto {
    pub name: String,
    pub plugin_type: String,
    pub wasm_path: String,
    pub loaded_at: String,
    pub call_count: u64,
    pub error_count: u64,
    pub active_instances: usize,
    pub max_instances: usize,
}

/// Request body for loading a new plugin
#[derive(Debug, Deserialize)]
pub struct LoadPluginRequest {
    pub name: String,
    pub plugin_type: String,
    pub wasm_path: String,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default = "default_wasi")]
    pub wasi: bool,
    #[serde(default = "default_max_instances")]
    pub max_instances: usize,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wasi() -> bool {
    false
}

fn default_max_instances() -> usize {
    4
}

fn default_timeout_ms() -> u64 {
    500
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// List all loaded plugins
async fn list_plugins(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
) -> Result<Json<PluginListResponse>> {
    require_scope(&auth, "admin")?;

    let manager = state
        .plugin_manager
        .as_ref()
        .ok_or_else(|| Error::ServiceUnavailable("Plugin system not initialized".to_string()))?;

    let plugins = manager.list_plugins();
    let dtos = plugins.into_iter().map(plugin_info_to_dto).collect();

    Ok(Json(PluginListResponse { plugins: dtos }))
}

/// Get info for a specific plugin
async fn get_plugin(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<PluginInfoDto>> {
    require_scope(&auth, "admin")?;

    let manager = state
        .plugin_manager
        .as_ref()
        .ok_or_else(|| Error::ServiceUnavailable("Plugin system not initialized".to_string()))?;

    let info = manager
        .plugin_info(&name)
        .ok_or_else(|| Error::NotFound(format!("Plugin not found: {name}")))?;

    Ok(Json(plugin_info_to_dto(info)))
}

/// Load a new plugin
async fn load_plugin(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Json(req): Json<LoadPluginRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "admin")?;

    let manager = state
        .plugin_manager
        .as_ref()
        .ok_or_else(|| Error::ServiceUnavailable("Plugin system not initialized".to_string()))?;

    let plugin_type = req
        .plugin_type
        .parse::<PluginType>()
        .map_err(Error::InvalidInput)?;

    let manifest = PluginManifest {
        name: req.name.clone(),
        plugin_type,
        wasm_path: req.wasm_path,
        config: req.config,
        wasi: req.wasi,
        max_instances: req.max_instances,
        timeout_ms: req.timeout_ms,
    };

    manager.load_plugin(manifest)?;

    tracing::info!(plugin_name = %req.name, "Plugin loaded successfully");

    Ok(Json(serde_json::json!({
        "message": "Plugin loaded successfully",
        "name": req.name,
    })))
}

/// Unload a plugin
async fn unload_plugin(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "admin")?;

    let manager = state
        .plugin_manager
        .as_ref()
        .ok_or_else(|| Error::ServiceUnavailable("Plugin system not initialized".to_string()))?;

    manager.unload_plugin(&name)?;

    tracing::info!(plugin_name = %name, "Plugin unloaded successfully");

    Ok(Json(serde_json::json!({
        "message": "Plugin unloaded successfully",
        "name": name,
    })))
}

/// Reload a plugin
async fn reload_plugin(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "admin")?;

    let manager = state
        .plugin_manager
        .as_ref()
        .ok_or_else(|| Error::ServiceUnavailable("Plugin system not initialized".to_string()))?;

    manager.reload_plugin(&name)?;

    tracing::info!(plugin_name = %name, "Plugin reloaded successfully");

    Ok(Json(serde_json::json!({
        "message": "Plugin reloaded successfully",
        "name": name,
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// Router
// ─────────────────────────────────────────────────────────────────────────────

/// Create the plugin admin router
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/plugins", get(list_plugins).post(load_plugin))
        .route(
            "/admin/plugins/{name}",
            get(get_plugin).delete(unload_plugin),
        )
        .route("/admin/plugins/{name}/reload", post(reload_plugin))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn plugin_info_to_dto(info: crate::features::plugin::domain::PluginInfo) -> PluginInfoDto {
    PluginInfoDto {
        name: info.name,
        plugin_type: info.plugin_type.to_string(),
        wasm_path: info.wasm_path,
        loaded_at: info.loaded_at.to_rfc3339(),
        call_count: info.call_count,
        error_count: info.error_count,
        active_instances: info.active_instances,
        max_instances: info.max_instances,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_compiles() {
        let _router = router();
    }
}
