//! Owner- and team-scoped access to persisted debug sessions.
//!
//! Live OpenCode access is intentionally not exposed here: it requires a trusted
//! Job credential binding and independently enforced model budget.
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use serde::Deserialize;
use thalamus_debug::store::{DebugEvent, DebugIdentity, DebugSession, DebugStore, StoreError};
use thalamus_identity::VerifiedPrincipal;
use uuid::Uuid;

use crate::{ApiError, AppState};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/debug/sessions/{id}", get(get_session))
        .route("/debug/sessions/{id}/events", get(get_events))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionQuery {
    team_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    team_id: Uuid,
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    100
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, "invalid debug request"),
        StoreError::Forbidden | StoreError::NotFound | StoreError::InvalidCredential => {
            ApiError::new(StatusCode::NOT_FOUND, "debug session not found")
        }
        StoreError::IdempotencyConflict
        | StoreError::InvalidTransition
        | StoreError::Inactive
        | StoreError::BudgetExhausted
        | StoreError::InitializationBusy => {
            ApiError::new(StatusCode::CONFLICT, "debug session state conflict")
        }
        StoreError::Database(_)
        | StoreError::SequenceExhausted
        | StoreError::RandomUnavailable
        | StoreError::CorruptData => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug storage unavailable")
        }
    }
}

fn principal<'a>(principal: &'a VerifiedPrincipal<'_>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(token) => VerifiedPrincipal::Session(token),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

/// Resolve the session under an identity guard, verify service policy, and then
/// reload it under a new guard. Store operations in the guarded transaction avoid
/// taking a second identity lock on another connection.
async fn authorized_session(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
) -> Result<DebugSession, ApiError> {
    let store = DebugStore::new(state.db.clone());
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    guard.commit().await?;

    let source = state.policy_source.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    let policy = source.load().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    let service = policy
        .service_id(&session.service_id)
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            )
        })?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "debug session not found"))?;
    let teams = policy
        .authorized_teams(&state.identity, principal(caller), &service)
        .await
        .map_err(|error| match error {
            thalamus_access::Error::Identity(error) => error.into(),
            thalamus_access::Error::Forbidden => {
                ApiError::new(StatusCode::NOT_FOUND, "debug session not found")
            }
            thalamus_access::Error::Source => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            ),
        })?;
    if !teams.contains(&team) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    Ok(session)
}

pub(crate) async fn session_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
) -> Result<DebugSession, ApiError> {
    let checked = authorized_session(state, caller, team, id).await?;
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = DebugStore::new(state.db.clone())
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    guard.commit().await?;
    Ok(session)
}

pub(crate) async fn events_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
    after: i64,
    limit: i64,
) -> Result<Vec<DebugEvent>, ApiError> {
    let checked = authorized_session(state, caller, team, id).await?;
    let store = DebugStore::new(state.db.clone());
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    let events = store
        .events_in_transaction(guard.transaction(), &identity, id, after, limit)
        .await
        .map_err(store_error)?;
    guard.commit().await?;
    Ok(events)
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<DebugSession>, ApiError> {
    session_for(&state, &caller.principal(), query.team_id, id)
        .await
        .map(Json)
}

async fn get_events(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<DebugEvent>>, ApiError> {
    events_for(
        &state,
        &caller.principal(),
        query.team_id,
        id,
        query.after,
        query.limit,
    )
    .await
    .map(Json)
}
