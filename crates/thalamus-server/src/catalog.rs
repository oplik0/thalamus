//! Read-only views of the GitOps service catalog and live Prometheus samples.
//!
//! Catalog intent is not evidence of acceptance, reconciliation, readiness,
//! validation, or benchmarking. Those observations remain unknown until a
//! trusted collector can associate evidence with the service's revision.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path as FilePath, PathBuf},
    sync::Arc,
};

use axum::{
    extract::{rejection::PathRejection, Path, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use thalamus_core::{ServiceDeployment, StatusValue};
use thalamus_identity::VerifiedPrincipal;
use thalamus_metrics::{Observation, Percentile, Preset, PrometheusClient, Sample, Target};
use tokio::task::JoinSet;

use crate::{ApiError, AppState, RequestPrincipal};

/// A service file in the KCL catalog. The renderer's service schema contains
/// deployments inline and has one namespace for the whole service.
#[derive(Debug, Deserialize)]
pub(crate) struct CatalogService {
    pub name: String,
    pub namespace: String,
    pub deployments: Vec<CatalogDeployment>,
    #[serde(skip)]
    pub source: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(from = "RawDeployment")]
pub(crate) struct CatalogDeployment {
    pub routing: ServiceDeployment,
    pub artifact: String,
    pub hardware: String,
}

#[derive(Deserialize)]
struct RawDeployment {
    name: String,
    artifact: String,
    hardware: String,
    // This is the default from templates/serving/kserve.k, not an observed weight.
    #[serde(default = "default_weight")]
    weight: u32,
}

fn default_weight() -> u32 {
    1
}

impl From<RawDeployment> for CatalogDeployment {
    fn from(raw: RawDeployment) -> Self {
        Self {
            routing: ServiceDeployment {
                deployment: raw.name,
                weight: raw.weight,
            },
            artifact: raw.artifact,
            hardware: raw.hardware,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CatalogError {
    #[error("catalog I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid catalog service {path:?}: {error}")]
    Yaml {
        path: PathBuf,
        error: serde_yaml::Error,
    },
    #[error("invalid service catalog: {0}")]
    Invalid(String),
}

fn parse_service(contents: &[u8], source: PathBuf) -> Result<CatalogService, CatalogError> {
    let mut service: CatalogService =
        serde_yaml::from_slice(contents).map_err(|error| CatalogError::Yaml {
            path: source.clone(),
            error,
        })?;
    service.source = source;
    if service.name.trim().is_empty() || service.namespace.trim().is_empty() {
        return Err(CatalogError::Invalid(format!(
            "service name and namespace must be nonempty in {:?}",
            service.source
        )));
    }
    let mut names = BTreeSet::new();
    for deployment in &service.deployments {
        let name = &deployment.routing.deployment;
        if name.trim().is_empty()
            || deployment.artifact.trim().is_empty()
            || deployment.hardware.trim().is_empty()
        {
            return Err(CatalogError::Invalid(format!(
                "deployment name, artifact and hardware must be nonempty in {:?}",
                service.source
            )));
        }
        if !names.insert(name) {
            return Err(CatalogError::Invalid(format!(
                "duplicate deployment {name:?} in {:?}",
                service.source
            )));
        }
    }
    Ok(service)
}

/// Load direct `services/*.yaml` or `services/*.yml` files under the configured
/// catalog root. This does not validate the entire KCL schema or render it.
pub(crate) fn load_services(root: &FilePath) -> Result<Vec<CatalogService>, CatalogError> {
    let mut services = Vec::new();
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(root.join("services"))? {
        let entry = entry?;
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        ) {
            continue;
        }
        // GitOps likewise refuses symlinked catalog files.
        if !entry.file_type()?.is_file() {
            return Err(CatalogError::Invalid(format!(
                "service catalog entry is not a regular file: {path:?}"
            )));
        }
        let service = parse_service(&fs::read(&path)?, path)?;
        if !names.insert(service.name.clone()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate service name {:?}",
                service.name
            )));
        }
        services.push(service);
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

fn policy_error(error: thalamus_access::Error) -> ApiError {
    match error {
        thalamus_access::Error::Forbidden => {
            ApiError::new(StatusCode::NOT_FOUND, "service not found")
        }
        thalamus_access::Error::Identity(error) => error.into(),
        thalamus_access::Error::Source => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy is unavailable",
        ),
    }
}

fn copy_principal<'a>(principal: &VerifiedPrincipal<'a>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(session) => VerifiedPrincipal::Session(session),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

/// Policy and displayed service fields are derived from the same immutable
/// Git blobs; the mutable checkout is never a source for these read paths.
pub(crate) async fn committed_catalog(
    state: &AppState,
) -> Result<(thalamus_access::Policy, Vec<CatalogService>), ApiError> {
    let source = state.policy_source.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy is not configured",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let policy = source.load().map_err(policy_error)?;
        let services = parse_committed_services(policy.committed_services().map_err(policy_error)?)
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "committed service catalog is invalid",
                )
            })?;
        Ok((policy, services))
    })
    .await
    .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "catalog loader failed"))?
}

fn parse_committed_services(
    blobs: &[(String, Vec<u8>)],
) -> Result<Vec<CatalogService>, CatalogError> {
    let mut services = Vec::with_capacity(blobs.len());
    let mut names = BTreeSet::new();
    for (filename, contents) in blobs {
        let service = parse_service(contents, PathBuf::from("services").join(filename))?;
        if !names.insert(service.name.clone()) {
            return Err(CatalogError::Invalid("duplicate service name".into()));
        }
        services.push(service);
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

fn service_id(service: &CatalogService) -> thalamus_access::ServiceId {
    thalamus_access::ServiceId {
        namespace: service.namespace.clone(),
        name: service.name.clone(),
    }
}

async fn authorize_service(
    state: &AppState,
    policy: &thalamus_access::Policy,
    principal: &VerifiedPrincipal<'_>,
    service: &CatalogService,
) -> Result<(), thalamus_access::Error> {
    policy
        .authorize(
            &state.identity,
            copy_principal(principal),
            &service_id(service),
        )
        .await
}

fn find_service(services: Vec<CatalogService>, name: &str) -> Result<CatalogService, ApiError> {
    services
        .into_iter()
        .find(|service| service.name == name)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "service not found"))
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ServiceList {
    services: Vec<ServiceView>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ServiceView {
    name: String,
    deployments: Vec<DeploymentView>,
    status: ServiceStatus,
}

#[derive(Serialize, utoipa::ToSchema)]
struct DeploymentView {
    name: String,
    recipe: &'static str,
    artifact: String,
    hardware: String,
}

#[derive(Clone, Serialize, utoipa::ToSchema)]
struct StatusView {
    #[serde(serialize_with = "serialize_status")]
    #[schema(value_type = String)]
    state: StatusValue,
    observed_at: Option<DateTime<Utc>>,
    reason: Option<&'static str>,
}

fn serialize_status<S: Serializer>(value: &StatusValue, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match value {
        StatusValue::Yes => "true",
        StatusValue::No => "false",
        StatusValue::Unknown => "unknown",
        StatusValue::Stale => "stale",
    })
}

#[derive(Serialize, utoipa::ToSchema)]
struct ServiceStatus {
    accepted: StatusView,
    applied: StatusView,
    running: StatusView,
    functional: StatusView,
    measured: StatusView,
}

impl ServiceStatus {
    fn unknown() -> Self {
        let unknown = StatusView {
            state: StatusValue::Unknown,
            observed_at: None,
            reason: None,
        };
        Self {
            accepted: unknown.clone(),
            applied: unknown.clone(),
            running: unknown.clone(),
            functional: unknown.clone(),
            measured: unknown,
        }
    }
}

impl From<CatalogService> for ServiceView {
    fn from(service: CatalogService) -> Self {
        Self {
            name: service.name,
            deployments: service
                .deployments
                .into_iter()
                .map(|deployment| DeploymentView {
                    name: deployment.routing.deployment,
                    // The KCL deployment schema does not contain a recipe. The UI
                    // requires a string, so explicitly show absence rather than
                    // infer a recipe from its image or from the renderer.
                    recipe: "Unknown",
                    artifact: deployment.artifact,
                    hardware: deployment.hardware,
                })
                .collect(),
            status: ServiceStatus::unknown(),
        }
    }
}

pub(crate) async fn list_service_catalog(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
) -> Result<ServiceList, ApiError> {
    state
        .identity
        .authenticate_principal(copy_principal(&principal))
        .await?;
    let (policy, services) = committed_catalog(state).await?;
    let mut visible = Vec::new();
    for service in services {
        match authorize_service(state, &policy, &principal, &service).await {
            Ok(()) => visible.push(service.into()),
            Err(thalamus_access::Error::Forbidden) => {}
            Err(error) => return Err(policy_error(error)),
        }
    }
    policy.committed_services().map_err(policy_error)?;
    Ok(ServiceList { services: visible })
}

pub(crate) async fn get_service_context(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    name: &str,
) -> Result<ServiceView, ApiError> {
    state
        .identity
        .authenticate_principal(copy_principal(&principal))
        .await?;
    let (policy, services) = committed_catalog(state).await?;
    let service = find_service(services, name)?;
    authorize_service(state, &policy, &principal, &service)
        .await
        .map_err(policy_error)?;
    Ok(service.into())
}

#[utoipa::path(
    get, path = "/api/v1/services",
    responses((status = 200, body = ServiceList), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 503, description = "Catalog or identity service unavailable"))
)]
pub(crate) async fn list_services(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ServiceList>, ApiError> {
    Ok(Json(
        list_service_catalog(&state, caller.principal()).await?,
    ))
}

#[utoipa::path(
    get, path = "/api/v1/services/{id}",
    params(("id" = String, Path, description = "Service name")),
    responses((status = 200, body = ServiceView), (status = 400, description = "Invalid service ID"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Service not found"), (status = 503, description = "Catalog or identity service unavailable"))
)]
pub(crate) async fn service_detail(
    State(state): State<Arc<AppState>>,
    path: Result<Path<String>, PathRejection>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ServiceView>, ApiError> {
    let Path(name) =
        path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid service ID"))?;
    Ok(Json(
        get_service_context(&state, caller.principal(), &name).await?,
    ))
}

#[derive(Clone, Copy)]
struct MetricSpec {
    preset: Preset,
    name: &'static str,
    unit: &'static str,
}

// KV and GPU units are deliberately the exporter's native units; the metrics
// adapter does not assert a particular scale for either exporter.
const METRIC_SPECS: [MetricSpec; 12] = [
    MetricSpec {
        preset: Preset::TimeToFirstToken(Percentile::P50),
        name: "time_to_first_token_p50",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::TimeToFirstToken(Percentile::P90),
        name: "time_to_first_token_p90",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::TimeToFirstToken(Percentile::P95),
        name: "time_to_first_token_p95",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::TimeToFirstToken(Percentile::P99),
        name: "time_to_first_token_p99",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::InterTokenLatency(Percentile::P50),
        name: "inter_token_latency_p50",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::InterTokenLatency(Percentile::P90),
        name: "inter_token_latency_p90",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::InterTokenLatency(Percentile::P95),
        name: "inter_token_latency_p95",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::InterTokenLatency(Percentile::P99),
        name: "inter_token_latency_p99",
        unit: "s",
    },
    MetricSpec {
        preset: Preset::Throughput,
        name: "throughput",
        unit: "tokens/s",
    },
    MetricSpec {
        preset: Preset::Queue,
        name: "queue",
        unit: "requests",
    },
    MetricSpec {
        preset: Preset::KvCache,
        name: "kv_cache",
        unit: "native",
    },
    MetricSpec {
        preset: Preset::GpuUtilization,
        name: "gpu_utilization",
        unit: "native",
    },
];

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct MetricList {
    metrics: Vec<MetricView>,
}

#[derive(Serialize, utoipa::ToSchema)]
struct MetricView {
    deployment: String,
    name: &'static str,
    value: Option<f64>,
    unit: &'static str,
    observed_at: Option<DateTime<Utc>>,
    state: &'static str,
}

/// Presets aggregate to one instant-vector sample. Empty, ambiguous, native
/// histogram, warned, non-finite and malformed results cannot be presented as
/// a current numeric measurement. Use the source timestamp, not receipt time.
fn sample_value(sample: &Sample) -> Option<(f64, DateTime<Utc>)> {
    if !sample.warnings.is_empty() || sample.result.result_type != "vector" {
        return None;
    }
    let results = sample.result.result.as_array()?;
    if results.len() != 1 {
        return None;
    }
    let pair = results[0].get("value")?.as_array()?;
    if pair.len() != 2 {
        return None;
    }
    let value: f64 = pair[1].as_str()?.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let milliseconds = pair[0].as_f64()? * 1_000.0;
    if !milliseconds.is_finite()
        || milliseconds < i64::MIN as f64
        || milliseconds >= i64::MAX as f64
    {
        return None;
    }
    let observed_at = DateTime::from_timestamp_millis(milliseconds.round() as i64)?;
    Some((value, observed_at))
}

fn metric_view(deployment: String, spec: MetricSpec, observation: Observation) -> MetricView {
    let (measurement, state) = match observation {
        Observation::Current(sample) => match sample_value(&sample) {
            Some(value) => (Some(value), "current"),
            None => (None, "unknown"),
        },
        Observation::Stale { .. } => (None, "stale"),
        Observation::Unknown { .. } => (None, "unknown"),
    };
    MetricView {
        deployment,
        name: spec.name,
        value: measurement.as_ref().map(|(value, _)| *value),
        unit: spec.unit,
        observed_at: measurement.map(|(_, time)| time),
        state,
    }
}

pub(crate) async fn query_service_metrics(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    name: &str,
) -> Result<MetricList, ApiError> {
    state
        .identity
        .authenticate_principal(copy_principal(&principal))
        .await?;
    let (policy, services) = committed_catalog(state).await?;
    let service = find_service(services, name)?;
    authorize_service(state, &policy, &principal, &service)
        .await
        .map_err(policy_error)?;
    if service.deployments.is_empty() {
        return Ok(MetricList {
            metrics: Vec::new(),
        });
    }
    let client = PrometheusClient::new(&state.config.prometheus_url).map_err(|error| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Prometheus configuration: {error}"),
        )
    })?;

    let count = service.deployments.len() * METRIC_SPECS.len();
    let mut ordered: Vec<Option<MetricView>> = (0..count).map(|_| None).collect();
    let mut next = 0;
    let mut tasks = JoinSet::new();
    loop {
        // Limit outstanding Prometheus requests without serializing all of the
        // latency percentiles or making response order depend on completion order.
        while tasks.len() < 12 {
            if next == count {
                break;
            }
            let index = next;
            next += 1;
            let deployment = &service.deployments[index / METRIC_SPECS.len()];
            let spec = METRIC_SPECS[index % METRIC_SPECS.len()];
            let client = client.clone();
            let target = Target {
                namespace: service.namespace.clone(),
                service: service.name.clone(),
                deployment: deployment.routing.deployment.clone(),
            };
            tasks.spawn(async move {
                let observation = client.observe_preset(spec.preset, &target, None).await;
                (index, metric_view(target.deployment, spec, observation))
            });
        }
        let Some(result) = tasks.join_next().await else {
            break;
        };
        let (index, metric) = result.map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "metrics query task failed",
            )
        })?;
        ordered[index] = Some(metric);
    }
    let metrics = ordered
        .into_iter()
        .map(|value| {
            value.ok_or_else(|| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "metrics query task missing",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Do not disclose samples if membership or the published revision changed
    // while the network requests were in flight.
    authorize_service(state, &policy, &principal, &service)
        .await
        .map_err(policy_error)?;
    Ok(MetricList { metrics })
}

#[utoipa::path(
    get, path = "/api/v1/services/{id}/metrics",
    params(("id" = String, Path, description = "Service name")),
    responses((status = 200, body = MetricList), (status = 400, description = "Invalid service ID"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Service not found"), (status = 500, description = "Metrics query task failed"), (status = 503, description = "Catalog or identity service unavailable"))
)]
pub(crate) async fn service_metrics(
    State(state): State<Arc<AppState>>,
    path: Result<Path<String>, PathRejection>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<MetricList>, ApiError> {
    let Path(name) =
        path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid service ID"))?;
    Ok(Json(
        query_service_metrics(&state, caller.principal(), &name).await?,
    ))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/services", get(list_services))
        .route("/services/{id}", get(service_detail))
        .route("/services/{id}/metrics", get(service_metrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::time::SystemTime;
    use thalamus_metrics::QueryResult;

    fn sample(result_type: &str, result: Value) -> Sample {
        Sample {
            observed_at: SystemTime::UNIX_EPOCH,
            result: QueryResult {
                result_type: result_type.into(),
                result,
            },
            warnings: Vec::new(),
        }
    }

    fn fixture() -> CatalogService {
        parse_service(
            include_str!("../../../catalog/services/example-service.yaml").as_bytes(),
            PathBuf::from("services/example-service.yaml"),
        )
        .unwrap()
    }

    #[test]
    fn parses_actual_service_catalog_and_reports_independent_unknown_states() {
        let service = fixture();
        assert_eq!(service.name, "example-service");
        assert_eq!(service.namespace, "example-inference");
        assert_eq!(service.deployments.len(), 1);
        assert_eq!(
            service.deployments[0].routing.deployment,
            "example-deployment"
        );
        assert_eq!(service.deployments[0].routing.weight, 1);
        assert_eq!(service.deployments[0].artifact, "example-model");
        assert_eq!(service.deployments[0].hardware, "example-gpu");
        let view = serde_json::to_value(ServiceView::from(service)).unwrap();
        assert_eq!(view["deployments"][0]["recipe"], "Unknown");
        for field in ["accepted", "applied", "running", "functional", "measured"] {
            assert_eq!(
                view["status"][field],
                json!({
                    "state": "unknown", "observed_at": null, "reason": null
                })
            );
        }
    }

    #[test]
    fn missing_service_is_not_resolved_as_a_file_path() {
        use axum::response::IntoResponse;

        for name in ["missing", "../example-service"] {
            let error = find_service(vec![fixture()], name).unwrap_err();
            assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND);
        }
    }

    #[test]
    fn parses_multiple_inline_deployments_without_inventing_recipe_or_weight() {
        let service = parse_service(
            br#"
name: my-service
namespace: production
deployments:
  - name: gpu-one
    artifact: weights-one
    hardware: first-gpu
  - name: gpu-two
    artifact: weights-two
    hardware: second-gpu
    weight: 7
"#,
            "service.yaml".into(),
        )
        .unwrap();
        assert_eq!(service.deployments[0].routing.weight, 1); // KCL default
        assert_eq!(service.deployments[1].routing.weight, 7);
        let view = serde_json::to_value(ServiceView::from(service)).unwrap();
        assert_eq!(view["deployments"][0]["recipe"], "Unknown");
        assert_eq!(view["deployments"][1]["name"], "gpu-two");
        assert_eq!(view["deployments"][1]["artifact"], "weights-two");
        assert_eq!(view["deployments"][1]["hardware"], "second-gpu");
    }

    #[test]
    fn rejects_ambiguous_or_incomplete_catalog_deployments() {
        for contents in [
            "name: example\nnamespace: n\ndeployments:\n  - name: a\n    artifact: m\n    hardware: h\n  - name: a\n    artifact: m\n    hardware: h\n",
            "name: example\nnamespace: n\ndeployments:\n  - name: a\n    artifact: m\n",
            "name: example\nnamespace: ''\ndeployments: []\n",
        ] {
            assert!(parse_service(contents.as_bytes(), "service.yaml".into()).is_err());
        }
    }

    struct TempCatalog(PathBuf);
    impl TempCatalog {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("thalamus-catalog-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("services")).unwrap();
            Self(root)
        }
    }
    impl Drop for TempCatalog {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn committed_catalog_ignores_dirty_worktree_and_rejects_stale_policy() {
        use std::process::Command;
        let temp = TempCatalog::new();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&temp.0)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git command failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "user.name", "Test"]);
        fs::create_dir_all(temp.0.join("catalog/services")).unwrap();
        let file = temp.0.join("catalog/services/model.yaml");
        fs::write(&file, "name: model\nnamespace: tenant-a\naccess:\n  teams: [a4db7aae-fd26-4f09-b652-e3d75cd4ac21]\ndeployments:\n  - name: primary\n    artifact: committed\n    hardware: gpu\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "publish"]);
        let revision = git(&["rev-parse", "HEAD"]);
        let source = thalamus_access::PolicySource::new(&temp.0, "catalog", &revision);
        fs::write(
            &file,
            "name: forged\nnamespace: other\naccess: {teams: []}\ndeployments: []\n",
        )
        .unwrap();
        let policy = source.load().unwrap();
        let services = parse_committed_services(policy.committed_services().unwrap()).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "model");
        assert_eq!(services[0].namespace, "tenant-a");
        assert_eq!(services[0].deployments[0].artifact, "committed");
        assert!(policy
            .includes_deployment(&service_id(&services[0]), "primary")
            .unwrap());
        git(&["add", "."]);
        git(&["commit", "-qm", "advance"]);
        assert!(matches!(
            policy.committed_services(),
            Err(thalamus_access::Error::Source)
        ));
        assert!(matches!(source.load(), Err(thalamus_access::Error::Source)));
    }

    #[test]
    fn policy_errors_do_not_mask_source_or_identity_failure() {
        use axum::response::IntoResponse;
        assert_eq!(
            policy_error(thalamus_access::Error::Forbidden)
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            policy_error(thalamus_access::Error::Source)
                .into_response()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            policy_error(thalamus_access::Error::Identity(
                thalamus_identity::Error::Unauthorized
            ))
            .into_response()
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn loads_only_catalog_service_files_and_rejects_duplicate_names() {
        let temp = TempCatalog::new();
        let contents = include_str!("../../../catalog/services/example-service.yaml");
        fs::write(temp.0.join("services/one.yaml"), contents).unwrap();
        fs::write(temp.0.join("services/ignored.txt"), "not yaml").unwrap();
        let services = load_services(&temp.0).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].source, temp.0.join("services/one.yaml"));
        fs::write(temp.0.join("services/two.yml"), contents).unwrap();
        assert!(matches!(
            load_services(&temp.0),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_service_files() {
        let temp = TempCatalog::new();
        fs::write(temp.0.join("real.yaml"), "not important").unwrap();
        std::os::unix::fs::symlink(temp.0.join("real.yaml"), temp.0.join("services/link.yaml"))
            .unwrap();
        assert!(matches!(
            load_services(&temp.0),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn reads_finite_vector_value_with_the_source_timestamp() {
        let sample = sample(
            "vector",
            json!([{ "metric": {}, "value": [1720000000.25, "7.5"] }]),
        );
        let (value, timestamp) = sample_value(&sample).unwrap();
        assert_eq!(value, 7.5);
        assert_eq!(
            timestamp,
            DateTime::from_timestamp(1720000000, 250_000_000).unwrap()
        );
    }

    #[test]
    fn observed_zero_is_not_confused_with_missing_data() {
        let observed = sample("vector", json!([{ "value": [1720000000, "0"] }]));
        let metric = metric_view(
            "deployment".into(),
            METRIC_SPECS[0],
            Observation::Current(observed),
        );
        assert_eq!(metric.value, Some(0.0));
        assert!(metric.observed_at.is_some());
        assert_eq!(metric.state, "current");
    }

    #[test]
    fn absent_ambiguous_invalid_or_warned_samples_are_unknown_not_zero() {
        let samples = [
            sample("vector", json!([])),
            sample(
                "vector",
                json!([{ "value": [1, "1"] }, { "value": [1, "2"] }]),
            ),
            sample("vector", json!([{ "value": [1, "NaN"] }])),
            sample("vector", json!([{ "value": [1, "+Inf"] }])),
            sample("vector", json!([{ "value": [1] }])),
            sample("vector", json!([{ "histogram": [1, {}] }])),
            sample("scalar", json!([1, "5"])),
            sample("vector", json!([{ "value": [1e308, "5"] }])),
            Sample {
                warnings: vec!["partial result".into()],
                ..sample("vector", json!([{ "value": [1, "5"] }]))
            },
        ];
        for sample in samples {
            let metric = metric_view(
                "deployment".into(),
                METRIC_SPECS[0],
                Observation::Current(sample),
            );
            let view = serde_json::to_value(metric).unwrap();
            assert_eq!(view["value"], Value::Null);
            assert_eq!(view["observed_at"], Value::Null);
            assert_eq!(view["state"], "unknown");
        }
        let metric = metric_view(
            "deployment".into(),
            METRIC_SPECS[0],
            Observation::Unknown {
                attempted_at: SystemTime::UNIX_EPOCH,
                error: thalamus_metrics::Error::MissingData,
            },
        );
        assert!(metric.value.is_none());
        assert!(metric.observed_at.is_none());
        assert_eq!(metric.state, "unknown");
        let stale = metric_view(
            "deployment".into(),
            METRIC_SPECS[0],
            Observation::Stale {
                attempted_at: SystemTime::UNIX_EPOCH,
                previous: sample("vector", json!([{ "value": [1, "5"] }])),
                error: thalamus_metrics::Error::MissingData,
            },
        );
        assert_eq!(stale.state, "stale");
        assert!(stale.value.is_none());
        assert!(stale.observed_at.is_none());
    }
}
