use super::*;
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use thalamus_core::PublishedService;
use uuid::Uuid;

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn image(c: char) -> String {
    format!("registry.example/image@{}", digest(c))
}
fn ready_condition(ty: &str) -> Value {
    json!({"type":ty,"status":"True","observedGeneration":1,"reason":"Valid"})
}
fn pod_conditions() -> Value {
    json!([{"type":"Ready","status":"True"},{"type":"Initialized","status":"True"}])
}
fn owner(kind: &str, name: &str, uid: &str) -> Value {
    json!({"kind":kind,"name":name,"uid":uid,"controller":true,"apiVersion":"apps/v1"})
}
fn metadata(ns: &str, name: &str, uid: &str) -> Value {
    json!({"namespace":ns,"name":name,"uid":uid,"generation":1})
}
fn deployment(ns: &str, name: &str, uid: &str, owner: Option<Value>, template: Value) -> Value {
    let mut meta = metadata(ns, name, uid);
    if let Some(owner) = owner {
        meta["ownerReferences"] = json!([owner]);
    }
    json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":meta,
        "spec":{"replicas":1,"strategy":{"type":"Recreate"},"template":{"spec":template}},
        "status":{"observedGeneration":1,"replicas":1,"updatedReplicas":1,"readyReplicas":1,"availableReplicas":1}})
}
fn rs(ns: &str, name: &str, uid: &str, owner_name: &str, owner_uid: &str) -> Value {
    let mut meta = metadata(ns, name, uid);
    meta["ownerReferences"] = json!([owner("Deployment", owner_name, owner_uid)]);
    json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":meta})
}
pub(super) fn policy(route: &str, benchmark: bool, team: Uuid) -> Value {
    let re = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
    let caps = serde_json::to_string(&BTreeMap::from([(team.to_string(), 1000)])).unwrap();
    let windows = serde_json::to_string(&BTreeMap::from([(team.to_string(), "day")])).unwrap();
    let team_cap = format!("{caps}[apiKey.team]");
    let unit = format!("{windows}[apiKey.team]");
    let mut requirements = vec![];
    for field in ["key_id", "user", "creator"] {
        requirements.push(format!(
            "type(apiKey.{field}) == string && apiKey.{field}.matches(\"{re}\")"
        ));
    }
    requirements.extend([
        format!("type(apiKey.team) == string && apiKey.team.matches(\"{re}\") && apiKey.team in {caps}"),
        "type(apiKey.kind) == string && apiKey.kind in [\"personal\", \"project\", \"job\", \"debug\"]".into(),
        format!("(apiKey.project == null && apiKey.kind == \"personal\") || (type(apiKey.project) == string && apiKey.project.matches(\"{re}\"))"),
        "type(apiKey.allowed_services) == list && \"svc\" in apiKey.allowed_services".into(),
        "apiKey.expiry == null || (type(apiKey.expiry) == string && request.startTime < timestamp(apiKey.expiry))".into(),
        if benchmark { "(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget >= 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget >= 0))".into() }
        else { "(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget > 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget > 0))".into() },
        format!("(apiKey.project != null || apiKey.project_budget == null) && type(apiKey.team_budget) == int && apiKey.team_budget == {team_cap} && apiKey.budget_window == {unit}"),
        format!("apiKey.kind != \"job\" || (type(apiKey.run_id) == string && apiKey.run_id.matches(\"{re}\") && type(apiKey.deployment) == string && apiKey.deployment != \"\" && apiKey.run_kind in [\"validation\", \"benchmark\"] && apiKey.expiry != null && apiKey.budget != null)"),
        "apiKey.kind == \"job\" && apiKey.deployment == \"dep\"".into(),
        if benchmark { "apiKey.kind == \"job\" && apiKey.run_kind == \"benchmark\" && type(apiKey.request_budget) == int && apiKey.request_budget > 0 && apiKey.request_budget <= 1000000".into() }
        else { "apiKey.kind != \"job\" || apiKey.run_kind == \"validation\"".into() },
    ]);
    let descriptors = if benchmark {
        vec![
            json!({"entries":[{"name":"benchmark_key","expression":"apiKey.key_id"}],"unit":"Requests",
            "limitOverride":format!("{{\"unit\":{unit},\"requestsPerUnit\":apiKey.request_budget}}")}),
        ]
    } else {
        let key_limit = format!(
            "(apiKey.budget == null || apiKey.budget > {team_cap} ? {team_cap} : apiKey.budget)"
        );
        let project_limit = format!(
            "(apiKey.project_budget == null || apiKey.project_budget > {team_cap} ? {team_cap} : apiKey.project_budget)"
        );
        [
            ("key_id", "apiKey.key_id", key_limit),
            ("project", "apiKey.project", project_limit),
            ("team", "apiKey.team", team_cap),
        ]
        .into_iter()
        .map(|(name, expression, limit)| {
            json!({"entries":[{"name":name,"expression":expression}],"unit":"Tokens",
                "limitOverride":format!("{{\"unit\":{unit},\"requestsPerUnit\":{limit}}}") })
        })
        .collect()
    };
    json!({"apiVersion":"agentgateway.dev/v1alpha1","kind":"AgentgatewayPolicy","metadata":metadata("work",&format!("{route}-auth"),"policy"),
        "spec":{"targetRefs":[{"group":"gateway.networking.k8s.io","kind":"HTTPRoute","name":route}],"traffic":{
            "apiKeyAuthentication":{"mode":"Strict","configMapSelector":{"matchLabels":{"thalamus.dev/gateway-keys":"true"}}},
            "authorization":{"action":"Require","policy":{"matchExpressions":requirements}},
            "transformation":{"request":{"remove":["x-thalamus-backend"],"set":[
                {"name":"x-thalamus-user","value":"apiKey.user"},{"name":"x-thalamus-project","value":"apiKey.project"},
                {"name":"x-thalamus-team","value":"apiKey.team"}],"metadata":{"thalamus_service":"\"svc\""}}},
            "rateLimit":{"global":{"failureMode":"FailClosed","descriptors":descriptors}}}},
        "status":{"ancestors":[{"conditions":[ready_condition("Accepted")]}]}})
}
pub(super) fn fixture(benchmark: bool) -> (RuntimeConfig, LiveResources, String) {
    let team = Uuid::from_u128(1);
    let project = Uuid::from_u128(2);
    let revision = "a".repeat(64);
    let route_name = if benchmark {
        "dep-benchmark"
    } else {
        "dep-candidate"
    };
    let path = if benchmark {
        "/candidates/dep/v1/completions"
    } else {
        "/candidates/dep/v1/chat/completions"
    };
    let upstream = if benchmark {
        "/v1/completions"
    } else {
        "/v1/chat/completions"
    };
    let header = format!("svc/dep/{revision}");
    let config = RuntimeConfig {
        namespace: "work".into(),
        route_version: "v1".into(),
        backend_version: "v1alpha1".into(),
        policy_version: "v1alpha1".into(),
        pool_version: "v1".into(),
        serving_version: "v1alpha2".into(),
        source_version: "v1".into(),
        flux_version: "v1".into(),
        helm_version: "v2".into(),
        trusted_agentic_images: vec![],
        routes: BTreeMap::from([(
            "svc".into(),
            ServiceRoutes {
                public: "svc".into(),
                team_id: team,
                project_id: project,
                service: PublishedService {
                    name: "svc".into(),
                    protocols: BTreeSet::new(),
                    deployments: vec![],
                    access_policy: "access".into(),
                    owners: vec![],
                },
            },
        )]),
    };
    let mut filters = vec![
        json!({"type":"URLRewrite","urlRewrite":{"path":{"type":"ReplaceFullPath","replaceFullPath":upstream}}}),
    ];
    let backend_ref = if benchmark {
        filters.push(json!({"type":"ResponseHeaderModifier","responseHeaderModifier":{"set":[{"name":"x-thalamus-backend","value":header}]}}));
        json!({"group":"inference.networking.k8s.io","kind":"InferencePool","name":"dep-inference-pool","port":8000,"weight":1})
    } else {
        json!({"group":"agentgateway.dev","kind":"AgentgatewayBackend","name":"dep-backend","weight":1})
    };
    let route = json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute","metadata":metadata("work",route_name,"route"),
        "spec":{"hostnames":["svc.example"],"parentRefs":[{"name":"gw","namespace":"gateway"}],
            "rules":[{"matches":[{"method":"POST","path":{"type":"Exact","value":path}}],"filters":filters,"backendRefs":[backend_ref]}]},
        "status":{"parents":[{"parentRef":{"name":"gw","namespace":"gateway"},"conditions":[ready_condition("Accepted"),ready_condition("ResolvedRefs")]}]}});
    let gateway = json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"Gateway","metadata":metadata("gateway","gw","gateway-uid"),
        "spec":{"listeners":[{"name":"https","protocol":"HTTPS","port":443}]},
        "status":{"conditions":[ready_condition("Accepted"),ready_condition("Programmed")],"listeners":[{"name":"https","conditions":[ready_condition("Accepted"),ready_condition("Programmed")]}]}});
    let flux_labels = json!({"kustomize.toolkit.fluxcd.io/name":"apply","kustomize.toolkit.fluxcd.io/namespace":"flux"});
    let annotations = json!({"thalamus.ai/deployment":"dep","thalamus.ai/revision":revision,"thalamus.ai/model-manifest":"manifest","thalamus.ai/serving-kind":"raw"});
    let mut backend_meta = metadata("work", "dep-backend", "backend-uid");
    backend_meta["annotations"] = annotations.clone();
    backend_meta["labels"] = flux_labels.clone();
    let backend = json!({"apiVersion":"agentgateway.dev/v1alpha1","kind":"AgentgatewayBackend","metadata":backend_meta,
        "spec":{"ai":{"provider":{"custom":{"model":"svc","formats":[{"type":"Completions","path":"/v1/chat/completions"}],"backendRef":{"group":"inference.networking.k8s.io","kind":"InferencePool","name":"dep-inference-pool"}}}},
            "policies":{"transformation":{"response":{"set":[{"name":"x-thalamus-backend","value":serde_json::to_string(&header).unwrap()}]}}}}});
    let pool = json!({"apiVersion":"inference.networking.k8s.io/v1","kind":"InferencePool","metadata":metadata("work","dep-inference-pool","pool-uid"),
        "spec":{"selector":{"matchLabels":{"workload":"dep"}},"targetPorts":[{"number":8000}],"endpointPickerRef":{"kind":"Service","name":"dep-epp","port":{"number":9002},"failureMode":"FailClose"}}});
    let engine = json!({"name":"main","image":image('b'),"command":["vllm","serve","/opt/thalamus/model"],
        "args":["--served-model-name","svc","--max-model-len","4096","--tokenizer","/opt/thalamus/model","--enable-prefix-caching"],
        "env":[{"name":"HOME","value":"/tmp"},{"name":"HF_HOME","value":"/tmp/huggingface"},{"name":"HF_HUB_OFFLINE","value":"1"},{"name":"TRANSFORMERS_OFFLINE","value":"1"}],
        "securityContext":{"readOnlyRootFilesystem":true,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}},
        "resources":{"limits":{"nvidia.com/gpu":"1"}},"ports":[{"containerPort":8000,"protocol":"TCP"}],
        "readinessProbe":{"httpGet":{"path":"/health","port":8000}},"volumeMounts":[{"name":"shm","mountPath":"/dev/shm"},{"name":"tmp","mountPath":"/tmp"}]});
    let pod_spec = json!({"nodeSelector":{"accelerator":"gpu"},"containers":[engine],"volumes":[{"name":"shm","emptyDir":{}},{"name":"tmp","emptyDir":{}}]});
    let mut serving = deployment("work", "dep", "deployment-uid", None, pod_spec.clone());
    serving["metadata"]["annotations"] = annotations;
    serving["metadata"]["labels"] = flux_labels;
    let mut engine_meta = metadata("work", "dep-pod", "engine-pod");
    engine_meta["labels"] = json!({"workload":"dep"});
    engine_meta["ownerReferences"] = json!([owner("ReplicaSet", "dep-rs", "dep-rs-uid")]);
    let mut engine_pod_spec = pod_spec;
    engine_pod_spec["nodeName"] = json!("node-a");
    let engine_pod = json!({"apiVersion":"v1","kind":"Pod","metadata":engine_meta,"spec":engine_pod_spec,
        "status":{"phase":"Running","conditions":pod_conditions(),"containerStatuses":[{"name":"main","ready":true,"imageID":digest('b')}]}});
    let mut gateway_meta = metadata("gateway", "gateway-pod", "gateway-pod-uid");
    gateway_meta["labels"] = json!({"gateway.networking.k8s.io/gateway-name":"gw"});
    gateway_meta["ownerReferences"] = json!([owner("ReplicaSet", "gateway-rs", "gateway-rs-uid")]);
    let gateway_pod = json!({"apiVersion":"v1","kind":"Pod","metadata":gateway_meta,"spec":{"containers":[{"name":"gateway","image":image('c')}]},
        "status":{"phase":"Running","conditions":pod_conditions(),"containerStatuses":[{"name":"gateway","ready":true,"imageID":digest('c')}]}});
    let mut picker_meta = metadata("work", "epp-pod", "epp-pod-uid");
    picker_meta["labels"] = json!({"app":"epp"});
    picker_meta["ownerReferences"] = json!([owner("ReplicaSet", "epp-rs", "epp-rs-uid")]);
    let picker_pod = json!({"apiVersion":"v1","kind":"Pod","metadata":picker_meta,"spec":{"containers":[{"name":"epp","image":"registry/epp:v1","command":["epp"],"ports":[{"containerPort":9002}],"readinessProbe":{"grpc":{"port":9003}}}]},
        "status":{"phase":"Running","conditions":pod_conditions(),"containerStatuses":[{"name":"epp","ready":true,"imageID":digest('f')}]}});
    let picker = json!({"apiVersion":"v1","kind":"Service","metadata":metadata("work","dep-epp","picker-uid"),"spec":{"selector":{"app":"epp"},"ports":[{"port":9002}]}});
    let node = json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"node-a","uid":"node-uid","labels":{"accelerator":"gpu"}},
        "status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":{"nvidia.com/gpu":"1"}}});
    let manifest = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":metadata("work","manifest","manifest-uid"),"immutable":true,
        "data":{"manifest.json":serde_json::to_string(&json!({"image":image('b'),"modelPath":"/opt/thalamus/model","modelDigest":digest('d'),"tokenizerDigest":digest('e')})).unwrap()}});
    let applied = format!("main@sha1:{}", "a".repeat(40));
    let flux = json!({"apiVersion":"kustomize.toolkit.fluxcd.io/v1","kind":"Kustomization","metadata":metadata("flux","apply","flux-uid"),
        "spec":{"sourceRef":{"kind":"GitRepository","name":"repo"}},"status":{"conditions":[ready_condition("Ready")],
        "lastAppliedRevision":applied,"lastAttemptedRevision":applied,"inventory":{"entries":[
            {"id":"work_dep_apps_Deployment"},{"id":"work_dep-backend_agentgateway.dev_AgentgatewayBackend"},
            {"id":"work_dep-inference-pool_inference.networking.k8s.io_InferencePool"}]}}});
    let source = json!({"apiVersion":"source.toolkit.fluxcd.io/v1","kind":"GitRepository","metadata":metadata("flux","repo","source-uid"),
        "status":{"conditions":[ready_condition("Ready")],"artifact":{"revision":applied}}});
    let workloads = vec![
        rs("work", "dep-rs", "dep-rs-uid", "dep", "deployment-uid"),
        rs(
            "work",
            "epp-rs",
            "epp-rs-uid",
            "epp-deployment",
            "epp-deployment-uid",
        ),
        deployment(
            "work",
            "epp-deployment",
            "epp-deployment-uid",
            None,
            json!({"containers":[]}),
        ),
        rs(
            "gateway",
            "gateway-rs",
            "gateway-rs-uid",
            "gateway-deployment",
            "gateway-deployment-uid",
        ),
        deployment(
            "gateway",
            "gateway-deployment",
            "gateway-deployment-uid",
            Some(owner("Gateway", "gw", "gateway-uid")),
            json!({"containers":[]}),
        ),
    ];
    let resources = LiveResources {
        routes: vec![route],
        gateways: vec![gateway],
        policies: vec![policy(route_name, benchmark, team)],
        backends: vec![backend],
        pools: vec![pool],
        picker_services: vec![picker],
        serving: vec![serving],
        kustomizations: vec![flux],
        sources: vec![source],
        config_maps: vec![manifest],
        pods: vec![engine_pod, picker_pod, gateway_pod],
        nodes: vec![node],
        workloads,
        ..Default::default()
    };
    (config, resources, route_name.into())
}
fn observe(config: &RuntimeConfig, r: &LiveResources, route: &str) -> Result<RouteSnapshot> {
    join_route(config, "svc", route, r, Utc::now())
}

#[test]
fn candidate_observes_full_live_chain_and_actual_identity() {
    let (config, r, route) = fixture(false);
    let snapshot = observe(&config, &r, &route).unwrap();
    assert_eq!(snapshot.hostnames, vec!["svc.example"]);
    assert_eq!(snapshot.paths, vec!["/candidates/dep/v1/chat/completions"]);
    assert_eq!(snapshot.ports, vec![443]);
    assert_eq!(snapshot.weights.get("dep"), Some(&1));
    assert_eq!(
        snapshot.deployments[0]
            .runtime
            .image_digests
            .get("container/main"),
        Some(&digest('b'))
    );
    assert_eq!(snapshot.deployments[0].runtime.model_content, digest('d'));
    assert_eq!(snapshot.deployments[0].nodes, vec!["node-a"]);
    let mut later = snapshot.clone();
    later.observed_at = Utc::now() + chrono::Duration::seconds(1);
    assert!(snapshot.same_identity(&later));
}

#[test]
fn agentic_model_backend_requires_exact_reviewed_formats_and_route_map() {
    let (config, mut resources, route) = fixture(false);
    resources.backends[0]["spec"]["ai"]["provider"]["custom"]["formats"] = json!([
        {"type":"Completions","path":"/v1/chat/completions"},
        {"type":"Responses","path":"/v1/responses"},
        {"type":"Messages","path":"/v1/messages"}
    ]);
    resources.backends[0]["spec"]["policies"]["ai"] = json!({"routes": {
        "/v1/chat/completions":"Completions",
        "/v1/responses":"Responses",
        "/v1/messages":"Messages"
    }});
    assert!(observe(&config, &resources, &route).is_ok());
    resources.backends[0]["spec"]["policies"]["ai"]["routes"]["/v1/responses"] =
        json!("Completions");
    assert!(observe(&config, &resources, &route).is_err());
    resources.backends[0]["spec"]["policies"]["ai"]["routes"]["/v1/responses"] = json!("Responses");
    resources.backends[0]["spec"]["ai"]["provider"]["custom"]["formats"][2]["path"] =
        json!("/v1/old-messages");
    assert!(observe(&config, &resources, &route).is_err());
}

#[test]
fn benchmark_joins_real_pool_and_actual_header() {
    let (config, mut r, route) = fixture(true);
    let snapshot = observe(&config, &r, &route).unwrap();
    assert_eq!(snapshot.paths, vec!["/candidates/dep/v1/completions"]);
    assert_eq!(
        snapshot.deployments[0].backend,
        format!("svc/dep/{}", "a".repeat(64))
    );
    r.routes[0]["spec"]["rules"][0]["filters"][1]["responseHeaderModifier"]["set"][0]["value"] =
        json!("forged");
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn route_conflicts_and_unauthorized_policy_are_rejected() {
    let (config, mut r, route) = fixture(false);
    let mut competing = r.routes[0].clone();
    competing["metadata"]["namespace"] = json!("another");
    competing["metadata"]["name"] = json!("other");
    r.routes.push(competing);
    assert!(observe(&config, &r, &route).is_err());
    r.routes.pop();
    r.policies[0]["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"] =
        json!(["true"]);
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn missing_or_ambiguous_pod_images_and_revision_fail_closed() {
    let (config, mut r, route) = fixture(false);
    r.pods[0]["status"]["containerStatuses"][0]["imageID"] = Value::Null;
    assert!(observe(&config, &r, &route).is_err());
    r.pods[0]["status"]["containerStatuses"][0]["imageID"] = json!(digest('f'));
    assert!(observe(&config, &r, &route).is_err());
    r.pods[0]["status"]["containerStatuses"][0]["imageID"] = json!(digest('b'));
    r.serving[0]["metadata"]["annotations"]["thalamus.ai/revision"] = json!("c".repeat(64));
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn manifests_mounts_sources_and_node_evidence_are_required() {
    let (config, mut r, route) = fixture(false);
    r.config_maps[0]["immutable"] = json!(false);
    assert!(observe(&config, &r, &route).is_err());
    r.config_maps[0]["immutable"] = json!(true);
    r.pods[0]["spec"]["containers"][0]["volumeMounts"][0]["mountPath"] = json!("/opt/thalamus");
    assert!(observe(&config, &r, &route).is_err());
    r.pods[0]["spec"]["containers"][0]["volumeMounts"][0]["mountPath"] = json!("/dev/shm");
    r.sources[0]["status"]["artifact"]["revision"] =
        json!("main@sha1:0000000000000000000000000000000000000000");
    assert!(observe(&config, &r, &route).is_err());
    r.sources.clear();
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn gateway_must_be_pinned_and_nodes_must_match() {
    let (config, mut r, route) = fixture(false);
    r.pods[2]["spec"]["containers"][0]["image"] = json!("registry/gateway:latest");
    assert!(observe(&config, &r, &route).is_err());
    r.pods[2]["spec"]["containers"][0]["image"] = json!(image('c'));
    r.nodes[0]["metadata"]["labels"]["accelerator"] = json!("other");
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn job_runner_provenance_accepts_terminated_failure_but_not_foreign_pods() {
    let job = json!({"kind":"Job","metadata":metadata("work","job","job-uid")});
    let mut meta = metadata("work", "runner-pod", "runner-uid");
    meta["ownerReferences"] = json!([owner("Job", "job", "job-uid")]);
    let pod = json!({"kind":"Pod","metadata":meta,"spec":{"containers":[{"name":"runner","image":image('a')}]},
        "status":{"phase":"Failed","containerStatuses":[{"name":"runner","imageID":digest('a'),"state":{"terminated":{"exitCode":1}}}]}});
    let observed = join_runner(&job, std::slice::from_ref(&pod)).unwrap();
    assert_eq!(observed.job_uid, "job-uid");
    assert_eq!(observed.pod_owner_job_uid, "job-uid");
    assert_eq!(observed.image_id, digest('a'));
    assert!(join_runner(&job, &[pod.clone(), pod]).is_err());
    assert!(join_runner(&job, &[]).is_err());
}

#[test]
fn rollout_identity_allows_only_scaling_and_keeps_every_ready_replica() {
    let (config, mut resources, route) = fixture(false);
    let candidate = observe(&config, &resources, &route).unwrap();
    resources.serving[0]["spec"]["replicas"] = json!(2);
    for field in [
        "replicas",
        "updatedReplicas",
        "readyReplicas",
        "availableReplicas",
    ] {
        resources.serving[0]["status"][field] = json!(2);
    }
    let mut second = resources.pods[0].clone();
    second["metadata"]["name"] = json!("dep-pod-two");
    second["metadata"]["uid"] = json!("engine-pod-two");
    resources.pods.push(second);
    let promoted = observe(&config, &resources, &route).unwrap();
    assert_eq!(promoted.deployments[0].nodes, vec!["node-a", "node-a"]);
    assert!(candidate.deployments[0].same_serving_revision(&promoted.deployments[0]));
    assert!(!candidate.same_identity(&promoted));
    let last = resources.pods.len() - 1;
    resources.pods[last]["status"]["containerStatuses"][0]["imageID"] = json!(digest('c'));
    assert!(observe(&config, &resources, &route).is_err());
}

#[test]
fn runner_rejects_duplicate_container_statuses_and_missing_image() {
    let job = json!({"kind":"Job","metadata":metadata("work","job","job-uid")});
    let mut meta = metadata("work", "pod", "pod-uid");
    meta["ownerReferences"] = json!([owner("Job", "job", "job-uid")]);
    let mut pod = json!({"kind":"Pod","metadata":meta,"spec":{"containers":[{"name":"runner","image":image('a')}]},
        "status":{"containerStatuses":[{"name":"runner","imageID":digest('a')},{"name":"runner","imageID":digest('a')}]}});
    assert!(join_runner(&job, &[pod.clone()]).is_err());
    pod["status"]["containerStatuses"] = json!([{"name":"runner"}]);
    assert!(join_runner(&job, &[pod]).is_err());
}

#[test]
fn helm_owned_objects_require_current_release_chart_and_git_source() {
    let (config, mut r, route) = fixture(false);
    let helm_labels =
        json!({"helm.toolkit.fluxcd.io/name":"release","helm.toolkit.fluxcd.io/namespace":"flux"});
    r.serving[0]["metadata"]["labels"] = helm_labels.clone();
    r.backends[0]["metadata"]["labels"] = helm_labels.clone();
    r.pools[0]["metadata"]["labels"] = helm_labels;
    r.kustomizations[0]["status"]["inventory"]["entries"] =
        json!([{"id":"flux_release_helm.toolkit.fluxcd.io_HelmRelease"}]);
    let mut release_meta = metadata("flux", "release", "release-uid");
    release_meta["labels"] = json!({"kustomize.toolkit.fluxcd.io/name":"apply","kustomize.toolkit.fluxcd.io/namespace":"flux"});
    r.helm_releases.push(json!({"apiVersion":"helm.toolkit.fluxcd.io/v2","kind":"HelmRelease","metadata":release_meta,
        "spec":{"chart":{"spec":{"chart":"app","version":"1.2.3","sourceRef":{"kind":"HelmRepository","name":"charts"}}}},
        "status":{"conditions":[ready_condition("Ready")],"helmChart":"flux/chart","lastAttemptedRevision":"1.2.3"}}));
    r.helm_charts.push(json!({"apiVersion":"source.toolkit.fluxcd.io/v1","kind":"HelmChart","metadata":metadata("flux","chart","chart-uid"),
        "spec":{"chart":"app","version":"1.2.3","sourceRef":{"kind":"HelmRepository","name":"charts"}},
        "status":{"conditions":[ready_condition("Ready")],"artifact":{"revision":"1.2.3","digest":digest('c')}}}));
    r.chart_sources.push(json!({"apiVersion":"source.toolkit.fluxcd.io/v1","kind":"HelmRepository","metadata":metadata("flux","charts","charts-uid"),
        "spec":{"url":"https://charts.example"},"status":{"conditions":[ready_condition("Ready")]}}));
    assert!(observe(&config, &r, &route).is_ok());
    r.helm_charts[0]["status"]["artifact"]["revision"] = json!("1.2.4");
    assert!(observe(&config, &r, &route).is_err());
    r.helm_charts[0]["status"]["artifact"]["revision"] = json!("1.2.3");
    r.helm_releases[0]["status"]["conditions"][0]["status"] = json!("False");
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn direct_pool_must_match_backend_and_picker_port() {
    let (config, mut r, route) = fixture(true);
    r.routes[0]["spec"]["rules"][0]["backendRefs"][0]["name"] = json!("other-pool");
    assert!(observe(&config, &r, &route).is_err());
    r.routes[0]["spec"]["rules"][0]["backendRefs"][0]["name"] = json!("dep-inference-pool");
    r.picker_services[0]["spec"]["ports"][0]["port"] = json!(9999);
    assert!(observe(&config, &r, &route).is_err());
}

#[test]
fn promotion_comparison_ignores_only_proven_scaling_and_flux_deltas() {
    let (config, resources, route) = fixture(false);
    let candidate = observe(&config, &resources, &route).unwrap();
    let original = &candidate.deployments[0];
    let mut other = original.clone();
    other.flux_revision = format!("main@sha1:{}", "b".repeat(40));
    other.nodes = vec!["different-validated-node".into()];
    other.desired.rendered_resources = digest('9');
    assert!(original.same_serving_revision(&other));
    let mut in_run = candidate.clone();
    in_run.deployments[0] = other.clone();
    assert!(!candidate.same_identity(&in_run));
    other.engine_spec["args"][3] = json!("another-model");
    assert!(!original.same_serving_revision(&other));
    other = original.clone();
    other.runtime.model_content = digest('8');
    assert!(!original.same_serving_revision(&other));
    other = original.clone();
    other
        .runtime
        .image_digests
        .insert("container/main".into(), digest('7'));
    assert!(!original.same_serving_revision(&other));
    other = original.clone();
    other.rollout_configuration = digest('6');
    assert!(!original.same_serving_revision(&other));
    other = original.clone();
    other.runtime.effective_configuration = digest('5');
    assert!(!original.same_serving_revision(&other));
    let mut old_agentic = original.clone();
    old_agentic.underlying_model = Some(UnderlyingModelIdentity {
        rollout_configuration: original.rollout_configuration.clone(),
        runtime: original.runtime.clone(),
    });
    old_agentic
        .runtime
        .component_versions
        .insert("agentic-pod-uid/old-pod".into(), digest('a'));
    let mut new_agentic = original.clone();
    new_agentic.underlying_model = old_agentic.underlying_model.clone();
    new_agentic
        .runtime
        .component_versions
        .insert("agentic-pod-uid/new-pod".into(), digest('b'));
    assert!(old_agentic.same_serving_revision(&new_agentic));
    assert_ne!(old_agentic.runtime, new_agentic.runtime);
    assert!(!original.same_serving_revision(&old_agentic));
    assert!(!old_agentic.same_serving_revision(original));
    new_agentic
        .runtime
        .component_versions
        .insert("agentic-image".into(), digest('c'));
    assert!(!old_agentic.same_serving_revision(&new_agentic));
}
