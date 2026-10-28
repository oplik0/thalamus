//! Authenticated, team-scoped historical token usage.
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use thalamus_identity::VerifiedPrincipal;
use thalamus_usage::{DailyUsage, UsageStore};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ApiError, AppState};

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/usage", get(usage))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UsageQuery {
    team: String,
    project_id: Option<Uuid>,
    start: NaiveDate,
    /// Exclusive UTC date.
    end: NaiveDate,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct UsageResponse {
    start: NaiveDate,
    end: NaiveDate,
    unit: &'static str,
    usage: Vec<DailyUsage>,
}

fn validate_range(start: NaiveDate, end: NaiveDate) -> Result<(), ApiError> {
    let days = (end - start).num_days();
    if !(1..=366).contains(&days) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "usage range must be between 1 and 366 days",
        ));
    }
    Ok(())
}
fn filter_project(rows: &mut Vec<DailyUsage>, project: Option<Uuid>) {
    if let Some(project) = project {
        rows.retain(|row| row.project_id == project);
    }
}

fn principal<'a>(value: &VerifiedPrincipal<'a>) -> VerifiedPrincipal<'a> {
    match value {
        VerifiedPrincipal::Session(session) => VerifiedPrincipal::Session(session),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

#[utoipa::path(get, path = "/api/v1/usage", params(("team" = String, Query), ("project_id" = Option<Uuid>, Query), ("start" = NaiveDate, Query, description = "Inclusive UTC date"), ("end" = NaiveDate, Query, description = "Exclusive UTC date")), responses((status = 200, body = UsageResponse), (status = 400, description = "Invalid date range"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Team or project not found"), (status = 503, description = "Usage history unavailable")))]
pub(crate) async fn usage(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageResponse>, ApiError> {
    usage_for(&state, &caller.principal(), query)
        .await
        .map(Json)
}

/// Membership, project ownership and the history read share the authorization
/// transaction. Historical team and project attribution comes from stored usage,
/// never by joining a record to a project's current team.
pub(crate) async fn usage_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    query: UsageQuery,
) -> Result<UsageResponse, ApiError> {
    validate_range(query.start, query.end)?;
    let team = state
        .identity
        .list_teams_principal(principal(caller))
        .await?
        .into_iter()
        .find(|team| team.name == query.team)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "team not found"))?;
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team.id, false)
        .await?;
    if let Some(project) = query.project_id {
        let exists: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM identity_projects WHERE id=$1 AND team_id=$2 FOR SHARE",
        )
        .bind(project)
        .bind(team.id)
        .fetch_optional(&mut **guard.transaction())
        .await?;
        if exists.is_none() {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "project not found"));
        }
    }
    let actor = guard.actor_id();
    let mut rows = UsageStore::list_in_transaction(
        guard.transaction(),
        actor,
        team.id,
        query.start,
        query.end,
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "usage history unavailable"))?;
    filter_project(&mut rows, query.project_id);
    guard.commit().await?;
    Ok(UsageResponse {
        start: query.start,
        end: query.end,
        unit: "tokens",
        usage: rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn usage_range_is_exclusive_and_bounded() {
        let start = day(2024, 1, 1);
        assert!(validate_range(start, start).is_err());
        assert!(validate_range(start, day(2023, 12, 31)).is_err());
        assert!(validate_range(start, day(2024, 1, 2)).is_ok());
        assert!(validate_range(start, day(2025, 1, 1)).is_ok());
        assert!(validate_range(start, day(2025, 1, 2)).is_err());
    }

    #[test]
    fn project_filter_preserves_historical_attribution_and_unknowns() {
        let team = Uuid::new_v4();
        let project = Uuid::new_v4();
        let other = Uuid::new_v4();
        let timestamp = day(2024, 1, 2).and_hms_opt(1, 2, 3).unwrap().and_utc();
        let row = DailyUsage {
            day: day(2024, 1, 1),
            team_id: team,
            project_id: project,
            service: "model".into(),
            traffic_class: "batch".into(),
            input_tokens: None,
            output_tokens: Some(42.0),
            observed_at: timestamp,
            updated_at: timestamp,
        };
        let mut rows = vec![
            row.clone(),
            DailyUsage {
                project_id: other,
                ..row
            },
        ];
        filter_project(&mut rows, Some(project));
        assert_eq!(rows.len(), 1);
        let value = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(value["team_id"], team.to_string());
        assert_eq!(value["project_id"], project.to_string());
        assert!(value["input_tokens"].is_null());
        assert_eq!(value["output_tokens"], 42.0);
        assert_eq!(
            value["observed_at"],
            timestamp.to_rfc3339().replace("+00:00", "Z")
        );
    }
}
