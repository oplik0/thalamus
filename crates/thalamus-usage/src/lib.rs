//! Historical, display-only gateway token observations, never quota enforcement.
//!
//! The worker is trusted to configure the gateway Prometheus source and call
//! `ingest_day`; neither configuration nor ingestion is a tenant API. Series
//! must carry team_id and project_id UUIDs, service and traffic_class labels.
//! Additional labels (including key IDs) are never persisted. Attribution is
//! taken from each historical series, not the project's current team. A missing
//! row or null counter means unknown, not zero. Only complete UTC days are read.
//! Apply the identity migrations and then migrations/0001_usage.sql.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("not authorized for team")]
    Forbidden,
    #[error("invalid or incomplete Prometheus response: {0}")]
    Source(&'static str),
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct DailyUsage {
    pub day: NaiveDate,
    pub project_id: Uuid,
    pub team_id: Uuid,
    pub service: String,
    pub traffic_class: String,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub observed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct UsageStore {
    pool: PgPool,
}

impl UsageStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// `user_id` must come from an authenticated principal, not request JSON.
    /// The end date is exclusive; authorization is checked for the historical
    /// team, not the project's present team. No row is evidence of zero usage.
    pub async fn list(
        &self,
        user_id: Uuid,
        team_id: Uuid,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<DailyUsage>, Error> {
        let mut tx = self.pool.begin().await?;
        let rows = Self::list_in_transaction(&mut tx, user_id, team_id, start, end).await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Use the same transaction as an identity authorization guard to avoid
    /// deadlocks and authorization races. The caller must commit on success.
    pub async fn list_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        user_id: Uuid,
        team_id: Uuid,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<DailyUsage>, Error> {
        if end <= start || (end - start).num_days() > 366 {
            return Err(Error::Invalid("date range"));
        }
        // Synchronize with identity membership updates, which lock this user row.
        let user = sqlx::query(
            "SELECT disabled, platform_admin FROM identity_users WHERE id = $1 FOR UPDATE",
        )
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(user) = user else {
            return Err(Error::Forbidden);
        };
        if user.get::<bool, _>("disabled") {
            return Err(Error::Forbidden);
        }
        let admin: bool = user.get("platform_admin");
        let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM identity_memberships WHERE user_id = $1 AND team_id = $2)")
            .bind(user_id).bind(team_id).fetch_one(&mut **tx).await?;
        if !admin && !member {
            return Err(Error::Forbidden);
        }
        let rows = sqlx::query("SELECT day, team_id, project_id, service, traffic_class, input_tokens, output_tokens, observed_at, updated_at FROM usage_daily WHERE team_id = $1 AND day >= $2 AND day < $3 ORDER BY day, project_id, service, traffic_class")
            .bind(team_id).bind(start).bind(end).fetch_all(&mut **tx).await?;
        Ok(rows
            .into_iter()
            .map(|r| DailyUsage {
                day: r.get("day"),
                team_id: r.get("team_id"),
                project_id: r.get("project_id"),
                service: r.get("service"),
                traffic_class: r.get("traffic_class"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                observed_at: r.get("observed_at"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    /// Trusted worker retry selection. Completeness means only that all
    /// returned and previously observed dimensions had both complete counters;
    /// it cannot establish that entirely absent series were ever present.
    pub async fn next_incomplete_day(
        &self,
        start: NaiveDate,
        end: NaiveDate,
        observed_before: DateTime<Utc>,
    ) -> Result<Option<NaiveDate>, Error> {
        if end <= start {
            return Err(Error::Invalid("date range"));
        }
        Ok(sqlx::query_scalar("SELECT day FROM usage_ingestion_days WHERE NOT complete AND day >= $1 AND day < $2 AND observed_at < $3 ORDER BY observed_at, day LIMIT 1")
            .bind(start).bind(end).bind(observed_before).fetch_optional(&self.pool).await?)
    }

    /// Trusted background ingestion. Failed queries leave previously observed
    /// rows untouched; successful re-observation replaces the day's values,
    /// marking previously known but now absent series unknown. Concurrent
    /// ingestion of a day is serialized and all updates commit atomically.
    pub async fn ingest_day(
        &self,
        source: &PrometheusSource,
        day: NaiveDate,
    ) -> Result<usize, Error> {
        if day >= Utc::now().date_naive() {
            return Err(Error::Invalid("day must be complete"));
        }
        let start = day
            .and_hms_opt(0, 0, 0)
            .ok_or(Error::Invalid("day"))?
            .and_utc()
            .timestamp();
        let end = start.checked_add(86400).ok_or(Error::Invalid("day"))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("thalamus-usage:{day}"))
            .execute(&mut *tx)
            .await?;
        let inputs = source.query(&source.input_counter, start, end).await?;
        let outputs = source.query(&source.output_counter, start, end).await?;
        let mut grouped: HashMap<Key, (Option<f64>, Option<f64>)> = HashMap::new();
        for (k, v) in inputs {
            grouped.entry(k).or_insert((None, None)).0 = v;
        }
        for (k, v) in outputs {
            grouped.entry(k).or_insert((None, None)).1 = v;
        }
        let observed_at = Utc::now();
        sqlx::query("UPDATE usage_daily SET input_tokens = NULL, output_tokens = NULL, observed_at = $2, updated_at = now() WHERE day = $1")
            .bind(day).bind(observed_at).execute(&mut *tx).await?;
        for (key, (input, output)) in &grouped {
            sqlx::query("INSERT INTO usage_daily (day, team_id, project_id, service, traffic_class, input_tokens, output_tokens, observed_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (day,team_id,project_id,service,traffic_class) DO UPDATE SET input_tokens=EXCLUDED.input_tokens, output_tokens=EXCLUDED.output_tokens, observed_at=EXCLUDED.observed_at, updated_at=now()")
                .bind(day).bind(key.team_id).bind(key.project_id).bind(&key.service).bind(&key.traffic_class).bind(input).bind(output).bind(observed_at).execute(&mut *tx).await?;
        }
        let unknown: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM usage_daily WHERE day = $1 AND (input_tokens IS NULL OR output_tokens IS NULL))")
            .bind(day).fetch_one(&mut *tx).await?;
        let complete = !grouped.is_empty() && !unknown;
        sqlx::query("INSERT INTO usage_ingestion_days (day, observed_at, complete) VALUES ($1, $2, $3) ON CONFLICT (day) DO UPDATE SET observed_at = EXCLUDED.observed_at, complete = EXCLUDED.complete, updated_at = now()")
            .bind(day).bind(observed_at).bind(complete).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(grouped.len())
    }
}

/// Trusted operator configuration. The step must divide a day exactly and
/// should be at least the source scrape interval; stale raw samples are filtered
/// out with timestamp() so missing scrape telemetry is not extrapolated.
#[derive(Clone)]
pub struct PrometheusSource {
    client: reqwest::Client,
    url: reqwest::Url,
    input_counter: String,
    output_counter: String,
    step: u32,
}
impl PrometheusSource {
    pub fn new(
        url: &str,
        input_counter: &str,
        output_counter: &str,
        step_seconds: u32,
    ) -> Result<Self, Error> {
        let mut url = reqwest::Url::parse(url).map_err(|_| Error::Invalid("Prometheus URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Invalid("Prometheus URL"));
        }
        let path = format!("{}/api/v1/query_range", url.path().trim_end_matches('/'));
        url.set_path(&path);
        if !metric_name(input_counter)
            || !metric_name(output_counter)
            || input_counter == output_counter
        {
            return Err(Error::Invalid("counter metric names"));
        }
        if step_seconds == 0 || step_seconds > 3600 || 86400 % step_seconds != 0 {
            return Err(Error::Invalid("step"));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            url,
            input_counter: input_counter.into(),
            output_counter: output_counter.into(),
            step: step_seconds,
        })
    }
    async fn query(
        &self,
        metric: &str,
        start: i64,
        end: i64,
    ) -> Result<HashMap<Key, Option<f64>>, Error> {
        // The `and` retains the original series labels; the comparison drops
        // samples at least one step old instead of Prometheus's 5-minute lookback.
        let expression = format!(
            "{metric} and (time() - timestamp({metric}) < {})",
            self.step
        );
        let body: Response = self
            .client
            .get(self.url.clone())
            .query(&[
                ("query", expression),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("step", self.step.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if body.status != "success" || !body.warnings.is_empty() {
            return Err(Error::Source("query failed or returned warnings"));
        }
        let data = body.data.ok_or(Error::Source("missing data"))?;
        if data.result_type != "matrix" {
            return Err(Error::Source("expected matrix"));
        }
        aggregate(data.result, start, end, self.step)
    }
}
fn metric_name(s: &str) -> bool {
    let mut chars = s.bytes();
    matches!(chars.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_' | b':'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b':')
        && s.len() <= 256
}
#[derive(Deserialize)]
struct Response {
    status: String,
    data: Option<Data>,
    #[serde(default)]
    warnings: Vec<String>,
}
#[derive(Deserialize)]
struct Data {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<Series>,
}
#[derive(Deserialize)]
struct Series {
    metric: HashMap<String, String>,
    values: Vec<(f64, String)>,
}
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    team_id: Uuid,
    project_id: Uuid,
    service: String,
    traffic_class: String,
}
fn aggregate(
    series: Vec<Series>,
    start: i64,
    end: i64,
    step: u32,
) -> Result<HashMap<Key, Option<f64>>, Error> {
    let mut result = HashMap::new();
    for s in series {
        let get = |name| {
            s.metric
                .get(name)
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or(Error::Source("missing or invalid required label"))
        };
        let key = Key {
            team_id: Uuid::parse_str(get("team_id")?)
                .map_err(|_| Error::Source("invalid team_id"))?,
            project_id: Uuid::parse_str(get("project_id")?)
                .map_err(|_| Error::Source("invalid project_id"))?,
            service: get("service")?.clone(),
            traffic_class: get("traffic_class")?.clone(),
        };
        let value = delta(&s.values, start, end, step);
        result
            .entry(key)
            .and_modify(|total: &mut Option<f64>| {
                *total = match (*total, value) {
                    (Some(a), Some(b)) if (a + b).is_finite() => Some(a + b),
                    _ => None,
                };
            })
            .or_insert(value);
    }
    Ok(result)
}
fn delta(values: &[(f64, String)], start: i64, end: i64, step: u32) -> Option<f64> {
    if values.len() != ((end - start) / i64::from(step) + 1) as usize {
        return None;
    }
    let mut first = None;
    let mut previous = 0.0;
    for (index, (timestamp, raw)) in values.iter().enumerate() {
        if *timestamp != (start + index as i64 * i64::from(step)) as f64 {
            return None;
        }
        let value: f64 = raw.parse().ok()?;
        if !value.is_finite() || value < 0.0 || (index > 0 && value < previous) {
            return None;
        }
        if index == 0 {
            first = Some(value);
        }
        previous = value;
    }
    Some(previous - first?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(values: &[(i64, &str)]) -> Vec<(f64, String)> {
        values
            .iter()
            .map(|(t, v)| (*t as f64, (*v).to_owned()))
            .collect()
    }

    fn series(team: &str, key_id: &str, values: &[(i64, &str)]) -> Series {
        Series {
            metric: [
                ("team_id", team),
                ("project_id", "00000000-0000-0000-0000-000000000003"),
                ("service", "chat"),
                ("traffic_class", "interactive"),
                ("key_id", key_id),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
            values: samples(values),
        }
    }

    #[test]
    fn complete_counter_requires_every_boundary_and_rejects_resets() {
        assert_eq!(
            delta(&samples(&[(0, "5"), (60, "8"), (120, "13")]), 0, 120, 60),
            Some(8.0)
        );
        assert_eq!(delta(&samples(&[(0, "5"), (120, "13")]), 0, 120, 60), None);
        assert_eq!(
            delta(&samples(&[(0, "5"), (60, "1"), (120, "13")]), 0, 120, 60),
            None
        );
        assert_eq!(
            delta(&samples(&[(0, "5"), (60, "NaN"), (120, "13")]), 0, 120, 60),
            None
        );
        assert_eq!(
            delta(&samples(&[(0, "5"), (60, "8"), (119, "13")]), 0, 120, 60),
            None
        );
        assert_eq!(
            delta(&samples(&[(0, "5"), (60, "5"), (120, "5")]), 0, 120, 60),
            Some(0.0)
        );
    }

    #[test]
    fn aggregates_key_series_without_persisting_key_and_preserves_team() {
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let observations = aggregate(
            vec![
                series(a, "secret-a", &[(0, "1"), (60, "3")]),
                series(a, "secret-b", &[(0, "2"), (60, "7")]),
                series(b, "secret-c", &[(0, "1"), (60, "9")]),
            ],
            0,
            60,
            60,
        )
        .unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations
                .iter()
                .find(|(k, _)| k.team_id == Uuid::parse_str(a).unwrap())
                .unwrap()
                .1,
            &Some(7.0)
        );
        assert_eq!(
            observations
                .iter()
                .find(|(k, _)| k.team_id == Uuid::parse_str(b).unwrap())
                .unwrap()
                .1,
            &Some(8.0)
        );
        assert!(!format!("{observations:?}").contains("secret"));
    }

    #[test]
    fn one_incomplete_series_makes_entire_aggregate_unknown() {
        let team = "00000000-0000-0000-0000-000000000001";
        let result = aggregate(
            vec![
                series(team, "a", &[(0, "1"), (60, "4")]),
                series(team, "b", &[(0, "5")]),
            ],
            0,
            60,
            60,
        )
        .unwrap();
        assert_eq!(result.values().copied().collect::<Vec<_>>(), vec![None]);
        assert!(aggregate(vec![series("not-a-uuid", "a", &[])], 0, 60, 60).is_err());
        assert!(aggregate(vec![], 0, 60, 60).unwrap().is_empty());
    }

    #[test]
    fn metric_configuration_rejects_promql_injection_and_bad_steps() {
        assert!(PrometheusSource::new(
            "https://prom.example/prometheus",
            "gateway_input_total",
            "gateway_output_total",
            60
        )
        .is_ok());
        assert!(
            PrometheusSource::new("https://prom.example", "x{key=\"secret\"}", "y", 60).is_err()
        );
        assert!(PrometheusSource::new("https://prom.example", "x", "y", 61).is_err());
        assert!(PrometheusSource::new("https://prom.example", "x", "x", 60).is_err());
    }
}
