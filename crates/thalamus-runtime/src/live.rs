//! Concrete Kubernetes API collection. No request data is used as evidence.
use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use kube::{
    Api, Client,
    api::{ApiResource, DynamicObject, GroupVersionKind, ListParams},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::agentic::join_agentic_route;
use crate::observe::{
    LiveResources, annotation, array, controller_owner, invalid, join_route, join_runner, lookup,
    name, namespace, owner_chain, picker_name, pinned_image, selected, selector, service_selector,
    string,
};
use crate::{
    Error, FillerState, Result, RouteSnapshot, RunnerObservation, RuntimeConfig, join_filler_state,
};

#[derive(Clone)]
pub struct LiveObserver {
    client: Client,
    config: RuntimeConfig,
}

// A full Secret GET is required to inspect `immutable`, but credential fields
// are ignored by deserialization and never retained in observation records.
#[derive(serde::Deserialize)]
struct AgenticSecretView {
    metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
    immutable: Option<bool>,
}

fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}
fn dns_subdomain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(dns_label)
}
fn valid_version(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn kube_error(error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 404) {
        invalid("referenced Kubernetes object does not exist")
    } else {
        Error::Kubernetes(error)
    }
}
fn resource(group: &str, version: &str, kind: &str, plural: &str) -> Result<ApiResource> {
    if !valid_version(version) {
        return Err(invalid("invalid API version"));
    }
    let allowed = matches!(
        (group, kind, plural),
        ("gateway.networking.k8s.io", "HTTPRoute", "httproutes")
            | ("gateway.networking.k8s.io", "Gateway", "gateways")
            | (
                "agentgateway.dev",
                "AgentgatewayBackend",
                "agentgatewaybackends"
            )
            | (
                "agentgateway.dev",
                "AgentgatewayPolicy",
                "agentgatewaypolicies"
            )
            | (
                "inference.networking.k8s.io",
                "InferencePool",
                "inferencepools"
            )
            | (
                "serving.kserve.io",
                "LLMInferenceService",
                "llminferenceservices"
            )
            | (
                "kustomize.toolkit.fluxcd.io",
                "Kustomization",
                "kustomizations"
            )
            | (
                "source.toolkit.fluxcd.io",
                "GitRepository",
                "gitrepositories"
            )
            | (
                "source.toolkit.fluxcd.io",
                "OCIRepository",
                "ocirepositories"
            )
            | ("source.toolkit.fluxcd.io", "Bucket", "buckets")
            | (
                "source.toolkit.fluxcd.io",
                "HelmRepository",
                "helmrepositories"
            )
            | ("source.toolkit.fluxcd.io", "HelmChart", "helmcharts")
            | ("helm.toolkit.fluxcd.io", "HelmRelease", "helmreleases")
            | ("apps", "Deployment", "deployments")
            | ("apps", "ReplicaSet", "replicasets")
            | ("apps", "StatefulSet", "statefulsets")
            | ("batch", "Job", "jobs")
            | (
                "autoscaling",
                "HorizontalPodAutoscaler",
                "horizontalpodautoscalers"
            )
            | ("keda.sh", "ScaledObject", "scaledobjects")
            | ("", "Pod", "pods")
            | ("", "Node", "nodes")
            | ("", "Service", "services")
            | ("discovery.k8s.io", "EndpointSlice", "endpointslices")
            | ("", "ConfigMap", "configmaps")
    );
    if !allowed {
        return Err(invalid("unsupported Kubernetes resource"));
    }
    Ok(ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(group, version, kind),
        plural,
    ))
}

impl LiveObserver {
    pub fn new(client: Client, config: RuntimeConfig) -> Result<Self> {
        if !dns_label(&config.namespace)
            || [
                &config.route_version,
                &config.backend_version,
                &config.policy_version,
                &config.pool_version,
                &config.serving_version,
                &config.flux_version,
                &config.source_version,
                &config.helm_version,
            ]
            .iter()
            .any(|v| !valid_version(v))
        {
            return Err(invalid("invalid namespace or configured API version"));
        }
        for (service, routes) in &config.routes {
            if !dns_label(service)
                || routes.service.name != *service
                || routes.team_id.is_nil()
                || routes.project_id.is_nil()
                || !dns_label(&routes.public)
            {
                return Err(invalid("invalid configured service or public route"));
            }
        }
        let mut trusted = BTreeSet::new();
        for image in &config.trusted_agentic_images {
            pinned_image(image)?;
            if !trusted.insert(image) {
                return Err(invalid("duplicate trusted agentic image"));
            }
        }
        Ok(Self { client, config })
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
    /// The caller must first authorize this service/deployment against catalog
    /// or durable rollout state. Resource names are derived, not caller URLs.
    pub async fn observe_candidate(
        &self,
        service: &str,
        deployment: &str,
    ) -> Result<RouteSnapshot> {
        if !dns_label(deployment) {
            return Err(invalid("invalid deployment"));
        }
        self.observe_named(service, &format!("{deployment}-candidate"))
            .await
    }

    /// The caller must independently authorize this service and deployment.
    /// The instance and route are discovered from live Kubernetes objects.
    pub async fn observe_agentic_candidate(
        &self,
        service: &str,
        deployment: &str,
    ) -> Result<RouteSnapshot> {
        if !dns_label(deployment) {
            return Err(invalid("invalid deployment"));
        }
        self.observe_agentic(service, Some(deployment)).await
    }

    /// Observe the agentic route for the configured service's team.
    pub async fn observe_agentic_public(&self, service: &str) -> Result<RouteSnapshot> {
        self.observe_agentic(service, None).await
    }

    pub async fn observe_benchmark(
        &self,
        service: &str,
        deployment: &str,
    ) -> Result<RouteSnapshot> {
        if !dns_label(deployment) {
            return Err(invalid("invalid deployment"));
        }
        self.observe_named(service, &format!("{deployment}-benchmark"))
            .await
    }

    /// Read one actual object for diagnostics or ephemeral Git-render comparison.
    pub async fn get_resource(
        &self,
        namespace: &str,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        name: &str,
    ) -> Result<Value> {
        if !dns_label(namespace) || name.is_empty() || name.contains('/') {
            return Err(invalid("invalid Kubernetes resource coordinates"));
        }
        let api: Api<DynamicObject> = Api::namespaced_with(
            self.client.clone(),
            namespace,
            &resource(group, version, kind, plural)?,
        );
        let object = api.get(name).await.map_err(kube_error)?;
        Ok(serde_json::to_value(object)?)
    }

    async fn get_optional(
        &self,
        namespace: &str,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        name: &str,
    ) -> Result<Option<Value>> {
        let api: Api<DynamicObject> = Api::namespaced_with(
            self.client.clone(),
            namespace,
            &resource(group, version, kind, plural)?,
        );
        api.get_opt(name)
            .await
            .map_err(kube_error)?
            .map(serde_json::to_value)
            .transpose()
            .map_err(Error::from)
    }
    async fn list(
        &self,
        namespace: Option<&str>,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
    ) -> Result<Vec<Value>> {
        let ar = resource(group, version, kind, plural)?;
        let api: Api<DynamicObject> = match namespace {
            Some(namespace) => Api::namespaced_with(self.client.clone(), namespace, &ar),
            None => Api::all_with(self.client.clone(), &ar),
        };
        let list = api.list(&ListParams::default()).await.map_err(kube_error)?;
        let mut values = list
            .items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        values.sort_by(|a, b| {
            (namespace_of(a), name_of(a), kind_of(a)).cmp(&(
                namespace_of(b),
                name_of(b),
                kind_of(b),
            ))
        });
        Ok(values)
    }
    pub async fn list_pods(&self, namespace: &str) -> Result<Vec<Value>> {
        if !dns_label(namespace) {
            return Err(invalid("invalid pod namespace"));
        }
        self.list(Some(namespace), "", "v1", "Pod", "pods").await
    }

    /// Project only immutable Secret metadata; never retain Secret values.
    async fn get_secret_metadata(&self, namespace: &str, name: &str) -> Result<Value> {
        if !dns_label(namespace) || !dns_subdomain(name) {
            return Err(invalid("invalid Secret coordinates"));
        }
        // PartialObjectMetadata does not contain `immutable`. Deserialize the
        // response into a narrow view which ignores data and stringData.
        let request = http::Request::get(format!("/api/v1/namespaces/{namespace}/secrets/{name}"))
            .header("Accept", "application/json")
            .body(Vec::new())
            .map_err(|_| invalid("invalid Secret request"))?;
        let secret: AgenticSecretView = self.client.request(request).await.map_err(kube_error)?;
        let immutable = secret.immutable == Some(true);
        let metadata = secret.metadata;
        if !immutable {
            return Err(invalid("agentic Secret is not immutable"));
        }
        if metadata.name.as_deref() != Some(name)
            || metadata.namespace.as_deref() != Some(namespace)
            || metadata.deletion_timestamp.is_some()
        {
            return Err(invalid("Secret metadata identity is invalid or deleting"));
        }
        let uid = metadata
            .uid
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("Secret UID missing"))?;
        let resource_version = metadata
            .resource_version
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("Secret resourceVersion missing"))?;
        let creation_timestamp = metadata
            .creation_timestamp
            .ok_or_else(|| invalid("Secret creationTimestamp missing"))?;
        // Do not retain arbitrary Secret annotations or managedFields, which
        // can themselves contain sensitive administrator-supplied values.
        Ok(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "immutable": true,
            "metadata": {
                "name": name,
                "namespace": namespace,
                "uid": uid,
                "resourceVersion": resource_version,
                "creationTimestamp": creation_timestamp,
            }
        }))
    }

    async fn get_node(&self, name: &str) -> Result<Value> {
        if name.is_empty() || name.contains('/') {
            return Err(invalid("invalid Node name"));
        }
        let api: Api<DynamicObject> =
            Api::all_with(self.client.clone(), &resource("", "v1", "Node", "nodes")?);
        Ok(serde_json::to_value(
            api.get(name).await.map_err(kube_error)?,
        )?)
    }

    /// Two complete reads reject a resource-version/spec/status change rather
    /// than combining independently observed generations into one snapshot.
    pub async fn observe_route(&self, service: &str, route_name: &str) -> Result<RouteSnapshot> {
        let routes = self
            .config
            .routes
            .get(service)
            .ok_or_else(|| invalid("unknown service"))?;
        if routes.public != route_name {
            return Err(invalid("route is not the configured public route"));
        }
        self.observe_named(service, route_name).await
    }

    async fn observe_named(&self, service: &str, route_name: &str) -> Result<RouteSnapshot> {
        let first = self.collect_route(service, route_name).await?;
        let at = Utc::now();
        let snapshot = join_route(&self.config, service, route_name, &first, at)?;
        let second = self.collect_route(service, route_name).await?;
        if first != second {
            return Err(Error::Changed);
        }
        let again = join_route(&self.config, service, route_name, &second, at)?;
        if !snapshot.same_identity(&again) {
            return Err(Error::Changed);
        }
        Ok(snapshot)
    }

    async fn observe_agentic(
        &self,
        service: &str,
        deployment: Option<&str>,
    ) -> Result<RouteSnapshot> {
        let routes = self
            .config
            .routes
            .get(service)
            .ok_or_else(|| invalid("unknown service"))?;
        let direct_name = match deployment {
            Some(deployment) => format!("{deployment}-candidate"),
            None => routes.public.clone(),
        };

        // Each pass independently reads both the direct model chain and the
        // outer/inner agentic chain. Neither is supplied by the caller.
        let first_direct = self.collect_route(service, &direct_name).await?;
        let at = Utc::now();
        let direct = join_route(&self.config, service, &direct_name, &first_direct, at)?;
        let (route_name, first) = self
            .collect_agentic_route(service, deployment, &direct, &first_direct)
            .await?;
        let snapshot = join_agentic_route(&self.config, service, &route_name, &direct, &first, at)?;

        let second_direct = self.collect_route(service, &direct_name).await?;
        let direct_again = join_route(&self.config, service, &direct_name, &second_direct, at)?;
        let (route_again, second) = self
            .collect_agentic_route(service, deployment, &direct_again, &second_direct)
            .await?;
        if first_direct != second_direct
            || first != second
            || route_name != route_again
            || !direct.same_identity(&direct_again)
        {
            return Err(Error::Changed);
        }
        let again = join_agentic_route(
            &self.config,
            service,
            &route_again,
            &direct_again,
            &second,
            at,
        )?;
        if !snapshot.same_identity(&again) {
            return Err(Error::Changed);
        }
        Ok(snapshot)
    }

    pub async fn observe_runner(&self, job_name: &str) -> Result<RunnerObservation> {
        if !dns_label(job_name) {
            return Err(invalid("invalid Job name"));
        }
        let ns = &self.config.namespace;
        let job = self
            .get_resource(ns, "batch", "v1", "Job", "jobs", job_name)
            .await?;
        let job_uid = string(&job, "/metadata/uid")?;
        let pods = job_pods(self.list_pods(ns).await?, job_uid);
        let observation = join_runner(&job, &pods)?;
        let job_after = self
            .get_resource(ns, "batch", "v1", "Job", "jobs", job_name)
            .await?;
        let pods_after = job_pods(self.list_pods(ns).await?, job_uid);
        if job != job_after || pods != pods_after {
            return Err(Error::Changed);
        }
        Ok(observation)
    }

    /// Independently observe one authorized shared-pool member's filler drain.
    /// This is point-in-time evidence, not a lock against future scaling.
    pub async fn observe_filler_state(
        &self,
        service: &str,
        deployment: &str,
    ) -> Result<FillerState> {
        if !dns_label(deployment) || !self.config.routes.contains_key(service) {
            return Err(invalid("invalid service or deployment"));
        }
        let first = self.collect_filler(service, deployment).await?;
        let observed_at = Utc::now();
        let state = join_filler_state(&self.config, service, deployment, &first, observed_at)?;
        let second = self.collect_filler(service, deployment).await?;
        if first != second {
            return Err(Error::Changed);
        }
        let again = join_filler_state(&self.config, service, deployment, &second, observed_at)?;
        if state != again {
            return Err(Error::Changed);
        }
        Ok(state)
    }

    async fn collect_filler(&self, service: &str, deployment: &str) -> Result<LiveResources> {
        if !self.config.routes.contains_key(service) {
            return Err(invalid("unknown service"));
        }
        let ns = &self.config.namespace;
        let mut r = LiveResources::default();
        let backend = self
            .get_resource(
                ns,
                "agentgateway.dev",
                &self.config.backend_version,
                "AgentgatewayBackend",
                "agentgatewaybackends",
                &format!("{deployment}-backend"),
            )
            .await?;
        let pool_name = string(&backend, "/spec/ai/provider/custom/backendRef/name")?;
        let pool = self
            .get_resource(
                ns,
                "inference.networking.k8s.io",
                &self.config.pool_version,
                "InferencePool",
                "inferencepools",
                pool_name,
            )
            .await?;
        let serving = self
            .get_resource(ns, "apps", "v1", "Deployment", "deployments", deployment)
            .await?;
        let filler_name = format!("{deployment}-filler");
        let filler = self
            .get_resource(ns, "apps", "v1", "Deployment", "deployments", &filler_name)
            .await?;
        let filler_uid = string(&filler, "/metadata/uid")?.to_owned();
        self.collect_flux(&mut r, &serving).await?;
        self.collect_flux(&mut r, &filler).await?;
        push_unique(&mut r.backends, backend)?;
        push_unique(&mut r.pools, pool)?;
        push_unique(&mut r.serving, serving)?;
        push_unique(&mut r.serving, filler)?;
        let sets = self
            .list(Some(ns), "apps", "v1", "ReplicaSet", "replicasets")
            .await?;
        r.replicasets = sets
            .into_iter()
            .filter(|rs| {
                controlled_by(rs, "Deployment", &filler_uid) || filler_labels(rs, deployment)
            })
            .collect();
        let rs_uids: BTreeSet<_> = r
            .replicasets
            .iter()
            .map(|rs| string(rs, "/metadata/uid").map(str::to_owned))
            .collect::<Result<_>>()?;
        let rs_names: BTreeSet<_> = r
            .replicasets
            .iter()
            .map(|rs| name(rs).map(str::to_owned))
            .collect::<Result<_>>()?;
        r.pods = self
            .list_pods(ns)
            .await?
            .into_iter()
            .filter(|pod| {
                filler_labels(pod, deployment)
                    || controlled_by(pod, "Deployment", &filler_uid)
                    || pod
                        .pointer("/metadata/ownerReferences")
                        .and_then(Value::as_array)
                        .is_some_and(|refs| {
                            refs.iter().any(|owner| {
                                owner.get("controller").and_then(Value::as_bool) == Some(true)
                                    && owner.get("kind").and_then(Value::as_str)
                                        == Some("ReplicaSet")
                                    && owner
                                        .get("uid")
                                        .and_then(Value::as_str)
                                        .is_some_and(|id| rs_uids.contains(id))
                            })
                        })
            })
            .collect();
        let hpas = self
            .list(
                Some(ns),
                "autoscaling",
                "v2",
                "HorizontalPodAutoscaler",
                "horizontalpodautoscalers",
            )
            .await?;
        let scaled = self
            .list(
                Some(ns),
                "keda.sh",
                "v1alpha1",
                "ScaledObject",
                "scaledobjects",
            )
            .await?;
        for object in hpas {
            let target = string(&object, "/spec/scaleTargetRef/name")?;
            if target == filler_name.as_str() || rs_names.contains(target) {
                r.horizontal_autoscalers.push(object);
            }
        }
        for object in scaled {
            let target = string(&object, "/spec/scaleTargetRef/name")?;
            if target == filler_name.as_str() || rs_names.contains(target) {
                r.scaled_objects.push(object);
            }
        }
        sort_resources(&mut r);
        Ok(r)
    }

    async fn collect_agentic_route(
        &self,
        service: &str,
        deployment: Option<&str>,
        direct: &RouteSnapshot,
        direct_resources: &LiveResources,
    ) -> Result<(String, LiveResources)> {
        let ns = &self.config.namespace;
        let team = self
            .config
            .routes
            .get(service)
            .ok_or_else(|| invalid("unknown service"))?
            .team_id
            .to_string();
        let all_routes = self
            .list(
                None,
                "gateway.networking.k8s.io",
                &self.config.route_version,
                "HTTPRoute",
                "httproutes",
            )
            .await?;
        let (public, route) = discover_agentic_routes(&all_routes, ns, service, &team, deployment)?;
        let route_name = name(route)?.to_owned();
        let mut r = direct_resources.clone();
        push_unique(&mut r.routes, public.clone())?;
        push_unique(&mut r.routes, route.clone())?;
        self.collect_agentic_gateways(&mut r, public).await?;
        self.collect_agentic_gateways(&mut r, route).await?;

        // Derive the only permitted backend/Service names from the
        // independently observed model deployments and the discovered
        // instance, then verify the live route refs and static hosts.
        let instance = annotation(route, "thalamus.ai/agentic-instance")?;
        let mut expected_backends = BTreeMap::new();
        for target in &direct.deployments {
            if !dns_label(&target.deployment) {
                return Err(invalid("invalid direct deployment"));
            }
            let mut digest = Sha256::new();
            digest.update(service.as_bytes());
            digest.update(b"/");
            digest.update(instance.as_bytes());
            digest.update(b"/");
            digest.update(target.deployment.as_bytes());
            let digest = format!("{:x}", digest.finalize());
            let hash = &digest[..32];
            if expected_backends
                .insert(
                    format!("agentic-backend-{hash}"),
                    format!("agentic-pod-{hash}"),
                )
                .is_some()
            {
                return Err(invalid("duplicate agentic backend identity"));
            }
        }
        let mut backend_names = BTreeSet::new();
        for rule in array(route, "/spec/rules")? {
            for target in array(rule, "/backendRefs")? {
                if target.get("group").and_then(Value::as_str) != Some("agentgateway.dev")
                    || target.get("kind").and_then(Value::as_str) != Some("AgentgatewayBackend")
                    || target
                        .get("namespace")
                        .and_then(Value::as_str)
                        .is_some_and(|target_ns| target_ns != ns)
                {
                    return Err(invalid("unsupported agentic route backend"));
                }
                let backend_name = string(target, "/name")?;
                if !dns_label(backend_name) {
                    return Err(invalid("invalid agentic backend name"));
                }
                backend_names.insert(backend_name);
            }
        }
        if backend_names.is_empty()
            || backend_names.len() != expected_backends.len()
            || backend_names
                .iter()
                .any(|backend| !expected_backends.contains_key(*backend))
        {
            return Err(invalid(
                "agentic route targets differ from the direct deployments",
            ));
        }
        for backend_name in backend_names {
            let backend = self
                .get_resource(
                    ns,
                    "agentgateway.dev",
                    &self.config.backend_version,
                    "AgentgatewayBackend",
                    "agentgatewaybackends",
                    backend_name,
                )
                .await?;
            let service_name = expected_backends
                .get(backend_name)
                .ok_or_else(|| invalid("unexpected agentic backend"))?;
            if string(&backend, "/spec/static/host")?
                != format!("{service_name}.{ns}.svc.cluster.local")
            {
                return Err(invalid("agentic backend host differs from its deployment"));
            }
            let pod_service = self
                .get_resource(ns, "", "v1", "Service", "services", service_name)
                .await?;
            let pod_deployment = self
                .get_resource(ns, "apps", "v1", "Deployment", "deployments", service_name)
                .await?;
            let labels = service_selector(&pod_service)?;
            if labels.len() != 1
                || labels
                    .get("thalamus.dev/agentic-instance")
                    .map(String::as_str)
                    != Some(service_name.as_str())
                || pod_deployment.pointer("/spec/selector/matchLabels")
                    != pod_service.pointer("/spec/selector")
            {
                return Err(invalid(
                    "agentic Service does not select its expected Deployment",
                ));
            }
            push_unique(&mut r.backends, backend)?;
            push_unique(&mut r.agentic_services, pod_service)?;
            push_unique(&mut r.agentic_deployments, pod_deployment)?;
        }

        // Internal routes are identified from the independently joined direct
        // deployments. Their live parentRefs determine the private Gateway and
        // corresponding Service; no startup inventory supplies either name.
        if direct.deployments.is_empty() {
            return Err(invalid("direct route has no deployments"));
        }
        for item in &direct.deployments {
            let internal_name = format!("{}-agentic-internal", item.deployment);
            let models_name = format!("{}-agentic-models", item.deployment);
            if !dns_label(&item.deployment)
                || !dns_label(&internal_name)
                || !dns_label(&models_name)
            {
                return Err(invalid("invalid internal agentic route name"));
            }
            let internal = lookup(&all_routes, ns, &internal_name)?;
            let parents = array(internal, "/spec/parentRefs")?;
            if parents.len() != 1 {
                return Err(invalid("internal agentic route must have one Gateway"));
            }
            let parent = &parents[0];
            let parent_ns = parent
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or(ns);
            if parent_ns != ns {
                return Err(invalid(
                    "internal agentic Gateway is outside the workload namespace",
                ));
            }
            self.collect_agentic_gateways(&mut r, internal).await?;
            let gateway_service = self
                .get_resource(
                    ns,
                    "",
                    "v1",
                    "Service",
                    "services",
                    string(parent, "/name")?,
                )
                .await?;
            push_unique(&mut r.agentic_services, gateway_service)?;
            let models = lookup(&all_routes, ns, &models_name)?;
            if models.pointer("/spec/parentRefs") != internal.pointer("/spec/parentRefs")
                || models.pointer("/spec/hostnames") != internal.pointer("/spec/hostnames")
            {
                return Err(invalid(
                    "agentic models route uses a different internal Gateway",
                ));
            }
            self.collect_agentic_gateways(&mut r, models).await?;
            push_unique(&mut r.routes, models.clone())?;
            push_unique(&mut r.routes, internal.clone())?;
        }

        // Conflict checks need every route attached to any observed Gateway,
        // including routes in other namespaces, and every policy in each
        // relevant Gateway or workload namespace.
        let parents: BTreeSet<_> = r
            .gateways
            .iter()
            .map(|gateway| {
                (
                    namespace_of(gateway).to_owned(),
                    name_of(gateway).to_owned(),
                )
            })
            .collect();
        for other in all_routes {
            if route_attaches_to(&other, &parents)
                || (namespace_of(&other) == ns
                    && agentic_annotation(&other, "thalamus.ai/service") == Some(service)
                    && matches!(
                        agentic_annotation(&other, "thalamus.ai/agentic-role"),
                        Some("public" | "candidate")
                    ))
            {
                push_unique(&mut r.routes, other)?;
            }
        }
        let mut namespaces = BTreeSet::from([ns.clone()]);
        namespaces.extend(parents.iter().map(|(parent_ns, _)| parent_ns.clone()));
        for policy_ns in &namespaces {
            for policy in self
                .list(
                    Some(policy_ns),
                    "agentgateway.dev",
                    &self.config.policy_version,
                    "AgentgatewayPolicy",
                    "agentgatewaypolicies",
                )
                .await?
            {
                push_unique(&mut r.policies, policy)?;
            }
        }

        let service_names: BTreeSet<_> = r
            .agentic_services
            .iter()
            .map(|service| name(service).map(str::to_owned))
            .collect::<Result<_>>()?;
        for slice in self
            .list(
                Some(ns),
                "discovery.k8s.io",
                "v1",
                "EndpointSlice",
                "endpointslices",
            )
            .await?
        {
            let service_name = slice
                .pointer("/metadata/labels")
                .and_then(Value::as_object)
                .and_then(|labels| labels.get("kubernetes.io/service-name"))
                .and_then(Value::as_str);
            if service_name.is_some_and(|service_name| service_names.contains(service_name)) {
                push_unique(&mut r.agentic_endpoint_slices, slice)?;
            }
        }

        let service_selectors = r
            .agentic_services
            .iter()
            .map(|service| Ok((namespace(service)?.to_owned(), service_selector(service)?)))
            .collect::<Result<Vec<_>>>()?;
        let agentic_selectors = r
            .agentic_services
            .iter()
            .filter(|service| {
                r.agentic_deployments
                    .iter()
                    .any(|workload| name_of(workload) == name_of(service))
            })
            .map(|service| Ok((name(service)?.to_owned(), service_selector(service)?)))
            .collect::<Result<Vec<_>>>()?;
        for pod_ns in &namespaces {
            for pod in self.list_pods(pod_ns).await? {
                let service_match = service_selectors
                    .iter()
                    .any(|(selector_ns, labels)| selector_ns == pod_ns && selected(&pod, labels));
                let gateway_match = r.gateways.iter().any(|gateway| {
                    namespace_of(gateway) == pod_ns
                        && pod
                            .pointer("/metadata/labels")
                            .and_then(Value::as_object)
                            .and_then(|labels| labels.get("gateway.networking.k8s.io/gateway-name"))
                            .and_then(Value::as_str)
                            == name(gateway).ok()
                });
                if service_match || gateway_match {
                    if pod_ready(&pod) {
                        self.collect_owners(&mut r, &pod).await?;
                    }
                    push_unique(&mut r.pods, pod)?;
                }
            }
        }

        // Collect live external configuration references from both the
        // Deployment templates and the actual eligible agentic Pods.
        let mut config_map_names = BTreeSet::new();
        let mut secret_names = BTreeSet::new();
        for workload in &r.agentic_deployments {
            collect_pod_dependencies(
                workload,
                "/spec/template/spec",
                &mut config_map_names,
                &mut secret_names,
            )?;
        }
        for pod in &r.pods {
            if namespace_of(pod) != ns || !pod_ready(pod) {
                continue;
            }
            for (workload_name, labels) in &agentic_selectors {
                if selected(pod, labels) {
                    let workload = lookup(&r.agentic_deployments, ns, workload_name)?;
                    owner_chain(&r, pod, workload, true)?;
                    collect_pod_dependencies(
                        pod,
                        "/spec",
                        &mut config_map_names,
                        &mut secret_names,
                    )?;
                }
            }
        }
        for config_map_name in config_map_names {
            let config_map = self
                .get_resource(ns, "", "v1", "ConfigMap", "configmaps", &config_map_name)
                .await?;
            push_unique(&mut r.config_maps, config_map)?;
        }
        for secret_name in secret_names {
            let metadata = self.get_secret_metadata(ns, &secret_name).await?;
            push_unique(&mut r.agentic_secret_metadata, metadata)?;
        }
        sort_resources(&mut r);
        Ok((route_name, r))
    }

    async fn collect_agentic_gateways(&self, r: &mut LiveResources, route: &Value) -> Result<()> {
        let route_ns = namespace(route)?;
        let parents = array(route, "/spec/parentRefs")?;
        if parents.is_empty() {
            return Err(invalid("agentic route has no Gateway parent"));
        }
        for parent in parents {
            if parent
                .get("group")
                .and_then(Value::as_str)
                .unwrap_or("gateway.networking.k8s.io")
                != "gateway.networking.k8s.io"
                || parent
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("Gateway")
                    != "Gateway"
            {
                return Err(invalid("unsupported agentic route parent"));
            }
            let parent_ns = parent
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or(route_ns);
            let parent_name = string(parent, "/name")?;
            if !dns_label(parent_ns) || !dns_label(parent_name) {
                return Err(invalid("invalid agentic Gateway coordinates"));
            }
            let gateway = self
                .get_resource(
                    parent_ns,
                    "gateway.networking.k8s.io",
                    &self.config.route_version,
                    "Gateway",
                    "gateways",
                    parent_name,
                )
                .await?;
            push_unique(&mut r.gateways, gateway)?;
        }
        Ok(())
    }

    async fn collect_route(&self, service: &str, route_name: &str) -> Result<LiveResources> {
        let routes = self
            .config
            .routes
            .get(service)
            .ok_or_else(|| invalid("unknown service"))?;
        let derived = route_name
            .strip_suffix("-candidate")
            .or_else(|| route_name.strip_suffix("-benchmark"));
        if !dns_label(route_name)
            || (routes.public != route_name && !derived.is_some_and(dns_label))
        {
            return Err(invalid(
                "route is not the configured public or a derived deployment route",
            ));
        }
        let ns = &self.config.namespace;
        let mut r = LiveResources {
            routes: self
                .list(
                    None,
                    "gateway.networking.k8s.io",
                    &self.config.route_version,
                    "HTTPRoute",
                    "httproutes",
                )
                .await?,
            ..Default::default()
        };
        let route = lookup(&r.routes, ns, route_name)?.clone();
        let mut gateway_namespaces = BTreeSet::new();
        for parent in array(&route, "/spec/parentRefs")? {
            let parent_ns = parent
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or(ns);
            let parent_name = string(parent, "/name")?;
            let gateway = self
                .get_resource(
                    parent_ns,
                    "gateway.networking.k8s.io",
                    &self.config.route_version,
                    "Gateway",
                    "gateways",
                    parent_name,
                )
                .await?;
            push_unique(&mut r.gateways, gateway)?;
            gateway_namespaces.insert(parent_ns.to_owned());
        }
        let parents: BTreeSet<_> = r
            .gateways
            .iter()
            .map(|g| (namespace_of(g).to_owned(), name_of(g).to_owned()))
            .collect();
        r.routes.retain(|other| {
            (namespace_of(other) == ns.as_str() && name_of(other) == route_name)
                || other
                    .pointer("/spec/parentRefs")
                    .and_then(Value::as_array)
                    .is_some_and(|refs| {
                        refs.iter().any(|p| {
                            let parent_ns = p
                                .get("namespace")
                                .and_then(Value::as_str)
                                .unwrap_or(namespace_of(other));
                            let parent_name = p.get("name").and_then(Value::as_str).unwrap_or("");
                            parents.contains(&(parent_ns.to_owned(), parent_name.to_owned()))
                        })
                    })
        });
        let mut policy_namespaces = gateway_namespaces.clone();
        policy_namespaces.insert(ns.clone());
        for policy_ns in policy_namespaces {
            for policy in self
                .list(
                    Some(&policy_ns),
                    "agentgateway.dev",
                    &self.config.policy_version,
                    "AgentgatewayPolicy",
                    "agentgatewaypolicies",
                )
                .await?
            {
                push_unique(&mut r.policies, policy)?;
            }
        }
        let mut backend_names = BTreeSet::new();
        for rule in array(&route, "/spec/rules")? {
            for target in array(rule, "/backendRefs")? {
                match (
                    target.get("group").and_then(Value::as_str),
                    target.get("kind").and_then(Value::as_str),
                ) {
                    (Some("agentgateway.dev"), Some("AgentgatewayBackend")) => {
                        backend_names.insert(string(target, "/name")?.to_owned());
                    }
                    (Some("inference.networking.k8s.io"), Some("InferencePool")) => {
                        let deployment = route_name
                            .strip_suffix("-benchmark")
                            .ok_or_else(|| invalid("invalid benchmark route name"))?;
                        backend_names.insert(format!("{deployment}-backend"));
                    }
                    _ => return Err(invalid("unsupported route target")),
                }
            }
        }
        if backend_names.is_empty() {
            return Err(invalid("route has no backend"));
        }
        for backend_name in backend_names {
            let backend = self
                .get_resource(
                    ns,
                    "agentgateway.dev",
                    &self.config.backend_version,
                    "AgentgatewayBackend",
                    "agentgatewaybackends",
                    &backend_name,
                )
                .await?;
            let pool_name =
                string(&backend, "/spec/ai/provider/custom/backendRef/name")?.to_owned();
            let deployment = backend
                .pointer("/metadata/annotations")
                .and_then(Value::as_object)
                .and_then(|m| m.get("thalamus.ai/deployment"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("backend deployment annotation missing"))?
                .to_owned();
            let raw = backend
                .pointer("/metadata/annotations")
                .and_then(Value::as_object)
                .and_then(|m| m.get("thalamus.ai/serving-kind"))
                .and_then(Value::as_str)
                == Some("raw");
            let pool = self
                .get_resource(
                    ns,
                    "inference.networking.k8s.io",
                    &self.config.pool_version,
                    "InferencePool",
                    "inferencepools",
                    &pool_name,
                )
                .await?;
            let picker = self
                .get_resource(ns, "", "v1", "Service", "services", picker_name(&pool)?)
                .await?;
            push_unique(&mut r.picker_services, picker)?;
            let (group, version, kind, plural) = if raw {
                ("apps", "v1", "Deployment", "deployments")
            } else {
                (
                    "serving.kserve.io",
                    self.config.serving_version.as_str(),
                    "LLMInferenceService",
                    "llminferenceservices",
                )
            };
            let serving = self
                .get_resource(ns, group, version, kind, plural, &deployment)
                .await?;
            let manifest_name = serving
                .pointer("/metadata/annotations")
                .and_then(Value::as_object)
                .and_then(|m| m.get("thalamus.ai/model-manifest"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("manifest reference missing"))?;
            let manifest = self
                .get_resource(ns, "", "v1", "ConfigMap", "configmaps", manifest_name)
                .await?;
            self.collect_flux(&mut r, &serving).await?;
            if raw {
                let filler_name = format!("{deployment}-filler");
                if let Some(filler) = self
                    .get_optional(ns, group, version, kind, plural, &filler_name)
                    .await?
                {
                    self.collect_flux(&mut r, &filler).await?;
                    push_unique(&mut r.serving, filler)?;
                }
            }
            push_unique(&mut r.backends, backend)?;
            push_unique(&mut r.pools, pool)?;
            push_unique(&mut r.serving, serving)?;
            push_unique(&mut r.config_maps, manifest)?;
        }
        r.policies.retain(|policy| {
            policy
                .pointer("/spec/targetRefs")
                .and_then(Value::as_array)
                .is_some_and(|refs| {
                    refs.iter().any(|target| {
                        let target_name = target.get("name").and_then(Value::as_str).unwrap_or("");
                        let policy_ns = namespace_of(policy);
                        match target.get("kind").and_then(Value::as_str) {
                            Some("HTTPRoute") => {
                                policy_ns == ns.as_str() && target_name == route_name
                            }
                            Some("Gateway") => {
                                parents.contains(&(policy_ns.to_owned(), target_name.to_owned()))
                            }
                            Some("AgentgatewayBackend") => {
                                policy_ns == ns.as_str()
                                    && r.backends.iter().any(|b| name_of(b) == target_name)
                            }
                            Some("InferencePool") => {
                                policy_ns == ns.as_str()
                                    && r.pools.iter().any(|p| name_of(p) == target_name)
                            }
                            Some("Service") => {
                                policy_ns == ns.as_str()
                                    && r.picker_services.iter().any(|p| name_of(p) == target_name)
                            }
                            _ => false,
                        }
                    })
                })
        });
        let pool_selectors = r.pools.iter().map(selector).collect::<Result<Vec<_>>>()?;
        let picker_selectors = r
            .picker_services
            .iter()
            .map(service_selector)
            .collect::<Result<Vec<_>>>()?;
        let mut pod_namespaces = gateway_namespaces;
        pod_namespaces.insert(ns.clone());
        for pod_ns in pod_namespaces {
            for pod in self.list_pods(&pod_ns).await? {
                let pool_match =
                    pod_ns == ns.as_str() && pool_selectors.iter().any(|s| selected(&pod, s));
                let picker_match =
                    pod_ns == ns.as_str() && picker_selectors.iter().any(|s| selected(&pod, s));
                let gateway_match = r.gateways.iter().any(|g| {
                    namespace_of(g) == pod_ns.as_str()
                        && pod
                            .pointer("/metadata/labels")
                            .and_then(Value::as_object)
                            .and_then(|l| l.get("gateway.networking.k8s.io/gateway-name"))
                            .and_then(Value::as_str)
                            == name(g).ok()
                });
                if pool_match || picker_match || gateway_match {
                    push_unique(&mut r.pods, pod)?;
                }
            }
        }
        let pods = r.pods.clone();
        for pod in pods {
            let relevant_pool =
                namespace(&pod)? == ns.as_str() && pool_selectors.iter().any(|s| selected(&pod, s));
            let relevant_picker = namespace(&pod)? == ns.as_str()
                && picker_selectors.iter().any(|s| selected(&pod, s));
            let relevant_gateway = r.gateways.iter().any(|g| {
                namespace(g).ok() == namespace(&pod).ok()
                    && pod
                        .pointer("/metadata/labels")
                        .and_then(Value::as_object)
                        .and_then(|l| l.get("gateway.networking.k8s.io/gateway-name"))
                        .and_then(Value::as_str)
                        == name(g).ok()
            });
            if (relevant_pool || relevant_gateway || relevant_picker)
                && pod
                    .pointer("/status/conditions")
                    .and_then(Value::as_array)
                    .is_some_and(|c| {
                        c.iter().any(|c| {
                            c.get("type").and_then(Value::as_str) == Some("Ready")
                                && c.get("status").and_then(Value::as_str) == Some("True")
                        })
                    })
            {
                self.collect_owners(&mut r, &pod).await?;
                if relevant_pool {
                    let node = self.get_node(string(&pod, "/spec/nodeName")?).await?;
                    push_unique(&mut r.nodes, node)?;
                }
            }
        }
        sort_resources(&mut r);
        Ok(r)
    }

    async fn collect_flux(&self, r: &mut LiveResources, serving: &Value) -> Result<()> {
        let labels = serving
            .pointer("/metadata/labels")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("Flux labels missing"))?;
        let helm_name = labels
            .get("helm.toolkit.fluxcd.io/name")
            .and_then(Value::as_str);
        let helm_ns = labels
            .get("helm.toolkit.fluxcd.io/namespace")
            .and_then(Value::as_str);
        let mut owner = serving.clone();
        match (helm_name, helm_ns) {
            (Some(name), Some(ns)) => {
                let release = self
                    .get_resource(
                        ns,
                        "helm.toolkit.fluxcd.io",
                        &self.config.helm_version,
                        "HelmRelease",
                        "helmreleases",
                        name,
                    )
                    .await?;
                if release.pointer("/spec/chart/spec").is_some() {
                    let chart_ref = string(&release, "/status/helmChart")?;
                    let (chart_ns, chart_name) = chart_ref
                        .split_once('/')
                        .ok_or_else(|| invalid("invalid HelmChart reference"))?;
                    let chart = self
                        .get_resource(
                            chart_ns,
                            "source.toolkit.fluxcd.io",
                            &self.config.source_version,
                            "HelmChart",
                            "helmcharts",
                            chart_name,
                        )
                        .await?;
                    let kind = string(&chart, "/spec/sourceRef/kind")?;
                    let plural = match kind {
                        "HelmRepository" => "helmrepositories",
                        "GitRepository" => "gitrepositories",
                        "Bucket" => "buckets",
                        _ => return Err(invalid("unsupported HelmChart source")),
                    };
                    let source_ns = chart
                        .pointer("/spec/sourceRef/namespace")
                        .and_then(Value::as_str)
                        .unwrap_or(chart_ns);
                    let source = self
                        .get_resource(
                            source_ns,
                            "source.toolkit.fluxcd.io",
                            &self.config.source_version,
                            kind,
                            plural,
                            string(&chart, "/spec/sourceRef/name")?,
                        )
                        .await?;
                    push_unique(&mut r.chart_sources, source)?;
                    push_unique(&mut r.helm_charts, chart)?;
                } else if let Some(reference) = release.pointer("/spec/chartRef") {
                    if string(reference, "/kind")? != "OCIRepository" {
                        return Err(invalid("unsupported Helm chart reference"));
                    }
                    let source_ns = reference
                        .get("namespace")
                        .and_then(Value::as_str)
                        .unwrap_or(ns);
                    let source = self
                        .get_resource(
                            source_ns,
                            "source.toolkit.fluxcd.io",
                            &self.config.source_version,
                            "OCIRepository",
                            "ocirepositories",
                            string(reference, "/name")?,
                        )
                        .await?;
                    push_unique(&mut r.chart_sources, source)?;
                } else {
                    return Err(invalid("HelmRelease has no chart reference"));
                }
                owner = release.clone();
                push_unique(&mut r.helm_releases, release)?;
            }
            (None, None) => {}
            _ => return Err(invalid("incomplete HelmRelease owner labels")),
        }
        let owner_labels = owner
            .pointer("/metadata/labels")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("Flux owner labels missing"))?;
        let flux_name = owner_labels
            .get("kustomize.toolkit.fluxcd.io/name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Flux Kustomization label missing"))?;
        let flux_ns = owner_labels
            .get("kustomize.toolkit.fluxcd.io/namespace")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Flux namespace label missing"))?;
        let flux = self
            .get_resource(
                flux_ns,
                "kustomize.toolkit.fluxcd.io",
                &self.config.flux_version,
                "Kustomization",
                "kustomizations",
                flux_name,
            )
            .await?;
        if string(&flux, "/spec/sourceRef/kind")? != "GitRepository" {
            return Err(invalid("unsupported Flux source kind"));
        }
        let source_ns = flux
            .pointer("/spec/sourceRef/namespace")
            .and_then(Value::as_str)
            .unwrap_or(flux_ns);
        let source = self
            .get_resource(
                source_ns,
                "source.toolkit.fluxcd.io",
                &self.config.source_version,
                "GitRepository",
                "gitrepositories",
                string(&flux, "/spec/sourceRef/name")?,
            )
            .await?;
        push_unique(&mut r.kustomizations, flux)?;
        push_unique(&mut r.sources, source)?;
        Ok(())
    }

    async fn collect_owners(&self, r: &mut LiveResources, pod: &Value) -> Result<()> {
        let ns = namespace(pod)?;
        let mut current = pod.clone();
        let mut visited = BTreeSet::new();
        for _ in 0..6 {
            if current.get("kind").and_then(Value::as_str) == Some("Deployment")
                && current
                    .pointer("/metadata/ownerReferences")
                    .and_then(Value::as_array)
                    .is_none_or(|owners| {
                        owners
                            .iter()
                            .all(|o| o.get("controller").and_then(Value::as_bool) != Some(true))
                    })
            {
                return Ok(());
            }
            let owner = controller_owner(&current)?;
            let kind = string(owner, "/kind")?;
            let name = string(owner, "/name")?;
            if kind == "Gateway" || kind == "LLMInferenceService" {
                return Ok(());
            }
            let plural = match kind {
                "ReplicaSet" => "replicasets",
                "Deployment" => "deployments",
                "StatefulSet" => "statefulsets",
                _ => return Err(invalid("unsupported pod owner")),
            };
            if !visited.insert((kind.to_owned(), name.to_owned())) {
                return Err(invalid("owner cycle"));
            }
            let object = self
                .get_resource(ns, "apps", "v1", kind, plural, name)
                .await?;
            push_unique(&mut r.workloads, object.clone())?;
            if r.serving
                .iter()
                .any(|s| s.pointer("/metadata/uid") == object.pointer("/metadata/uid"))
            {
                return Ok(());
            }
            current = object;
        }
        Err(invalid("owner chain too deep"))
    }
}

fn agentic_annotation<'a>(route: &'a Value, key: &str) -> Option<&'a str> {
    route
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .and_then(|annotations| annotations.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn discover_agentic_routes<'a>(
    routes: &'a [Value],
    ns: &str,
    service: &str,
    team: &str,
    deployment: Option<&str>,
) -> Result<(&'a Value, &'a Value)> {
    let mut public = None;
    let mut candidate = None;
    for route in routes {
        if namespace_of(route) != ns
            || agentic_annotation(route, "thalamus.ai/service") != Some(service)
        {
            continue;
        }
        let role = agentic_annotation(route, "thalamus.ai/agentic-role");
        if role != Some("public") && (role != Some("candidate") || deployment.is_none()) {
            continue;
        }
        let route_team = annotation(route, "thalamus.ai/team")?;
        if route_team != "shared" && route_team != team {
            continue;
        }
        match role {
            Some("public") => {
                let instance = annotation(route, "thalamus.ai/agentic-instance")?;
                if !dns_label(instance) {
                    return Err(invalid("invalid agentic instance"));
                }
                if public.replace(route).is_some() {
                    return Err(invalid("ambiguous agentic public instance"));
                }
            }
            Some("candidate") => {
                if annotation(route, "thalamus.ai/deployment")? != deployment.unwrap_or("") {
                    continue;
                }
                if candidate.replace(route).is_some() {
                    return Err(invalid("ambiguous agentic candidate route"));
                }
            }
            _ => unreachable!(),
        }
    }
    let public = public.ok_or_else(|| invalid("agentic public instance not observed"))?;
    let instance = annotation(public, "thalamus.ai/agentic-instance")?;
    let public_name = format!("{service}-{instance}");
    if !dns_label(&public_name) || name(public)? != public_name {
        return Err(invalid(
            "agentic public route name differs from its instance",
        ));
    }
    let route = if deployment.is_some() {
        let candidate = candidate.ok_or_else(|| invalid("agentic candidate route not observed"))?;
        if annotation(candidate, "thalamus.ai/agentic-instance")? != instance
            || annotation(candidate, "thalamus.ai/team")? != annotation(public, "thalamus.ai/team")?
            || candidate.pointer("/spec/hostnames") != public.pointer("/spec/hostnames")
            || candidate.pointer("/spec/parentRefs") != public.pointer("/spec/parentRefs")
            || !dns_label(name(candidate)?)
        {
            return Err(invalid(
                "agentic candidate does not match the public instance",
            ));
        }
        candidate
    } else {
        public
    };
    Ok((public, route))
}

fn route_attaches_to(route: &Value, parents: &BTreeSet<(String, String)>) -> bool {
    route
        .pointer("/spec/parentRefs")
        .and_then(Value::as_array)
        .is_some_and(|refs| {
            refs.iter().any(|parent| {
                let parent_ns = parent
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or(namespace_of(route));
                let parent_name = parent.get("name").and_then(Value::as_str).unwrap_or("");
                parents
                    .iter()
                    .any(|(ns, name)| ns == parent_ns && name == parent_name)
            })
        })
}

fn pod_ready(pod: &Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

fn optional_items<'a>(object: &'a Value, key: &str) -> Result<&'a [Value]> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => Err(invalid(format!("invalid {key} array"))),
    }
}

fn collect_pod_dependencies(
    object: &Value,
    spec_path: &str,
    config_maps: &mut BTreeSet<String>,
    secrets: &mut BTreeSet<String>,
) -> Result<()> {
    let spec = object
        .pointer(spec_path)
        .filter(|spec| spec.is_object())
        .ok_or_else(|| invalid("agentic Pod spec missing"))?;
    // Bound Secret reads before issuing them: only the reviewed single
    // agentic container's database/search environment may reference Secrets.
    let containers = optional_items(spec, "containers")?;
    if containers.len() != 1
        || string(&containers[0], "/name")? != "agentic-api"
        || !optional_items(spec, "initContainers")?.is_empty()
        || !optional_items(spec, "ephemeralContainers")?.is_empty()
        || !optional_items(&containers[0], "envFrom")?.is_empty()
    {
        return Err(invalid("unsupported agentic container dependency"));
    }
    for volume in optional_items(spec, "volumes")? {
        if volume.get("secret").is_some() || volume.get("projected").is_some() {
            return Err(invalid("unsupported agentic Secret or projected volume"));
        }
        if volume.get("configMap").is_some() {
            config_maps.insert(string(volume, "/configMap/name")?.to_owned());
        }
    }
    for env in optional_items(&containers[0], "env")? {
        if env.get("valueFrom").is_none() {
            continue;
        }
        let variable = string(env, "/name")?;
        if !matches!(
            variable,
            "DATABASE_URL"
                | "YOU_API_KEY"
                | "BRAVE_API_KEY"
                | "TAVILY_API_KEY"
                | "SERPLY_API_KEY"
                | "SEARXNG_API_KEY"
        ) {
            return Err(invalid("unsupported agentic dynamic environment"));
        }
        let reference = env
            .pointer("/valueFrom/secretKeyRef")
            .ok_or_else(|| invalid("agentic Secret reference missing"))?;
        if env.get("value").is_some()
            || env
                .pointer("/valueFrom")
                .and_then(Value::as_object)
                .is_none_or(|v| v.len() != 1)
            || reference
                .get("optional")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || (variable == "DATABASE_URL" && string(reference, "/key")? != "DATABASE_URL")
        {
            return Err(invalid("invalid agentic Secret reference"));
        }
        string(reference, "/key")?;
        secrets.insert(string(reference, "/name")?.to_owned());
    }
    Ok(())
}

fn namespace_of(v: &Value) -> &str {
    v.pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .unwrap_or("")
}
fn name_of(v: &Value) -> &str {
    v.pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}
fn kind_of(v: &Value) -> &str {
    v.get("kind").and_then(Value::as_str).unwrap_or("")
}
fn push_unique(items: &mut Vec<Value>, value: Value) -> Result<()> {
    if let Some(existing) = items.iter().find(|v| {
        namespace_of(v) == namespace_of(&value)
            && name_of(v) == name_of(&value)
            && kind_of(v) == kind_of(&value)
    }) {
        if existing != &value {
            return Err(Error::Changed);
        }
    } else {
        items.push(value);
    }
    Ok(())
}
fn sort_resources(r: &mut LiveResources) {
    for items in [
        &mut r.routes,
        &mut r.gateways,
        &mut r.policies,
        &mut r.backends,
        &mut r.pools,
        &mut r.picker_services,
        &mut r.serving,
        &mut r.agentic_services,
        &mut r.agentic_deployments,
        &mut r.agentic_secret_metadata,
        &mut r.agentic_endpoint_slices,
        &mut r.kustomizations,
        &mut r.sources,
        &mut r.helm_releases,
        &mut r.helm_charts,
        &mut r.chart_sources,
        &mut r.config_maps,
        &mut r.pods,
        &mut r.nodes,
        &mut r.replicasets,
        &mut r.horizontal_autoscalers,
        &mut r.scaled_objects,
        &mut r.workloads,
    ] {
        items.sort_by(|a, b| {
            (namespace_of(a), name_of(a), kind_of(a)).cmp(&(
                namespace_of(b),
                name_of(b),
                kind_of(b),
            ))
        });
    }
}

fn job_pods(pods: Vec<Value>, job_uid: &str) -> Vec<Value> {
    pods.into_iter()
        .filter(|pod| {
            pod.pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        owner.get("kind").and_then(Value::as_str) == Some("Job")
                            && owner.get("controller").and_then(Value::as_bool) == Some(true)
                            && owner.get("uid").and_then(Value::as_str) == Some(job_uid)
                    })
                })
        })
        .collect()
}

fn controlled_by(value: &Value, kind: &str, uid: &str) -> bool {
    value
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|refs| {
            refs.iter().any(|owner| {
                owner.get("controller").and_then(Value::as_bool) == Some(true)
                    && owner.get("kind").and_then(Value::as_str) == Some(kind)
                    && owner.get("uid").and_then(Value::as_str) == Some(uid)
            })
        })
}
fn filler_labels(value: &Value, deployment: &str) -> bool {
    value
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .is_some_and(|labels| {
            labels.get("thalamus.ai/deployment").and_then(Value::as_str) == Some(deployment)
                && labels.get("thalamus.ai/pool-role").and_then(Value::as_str) == Some("filler")
        })
}
