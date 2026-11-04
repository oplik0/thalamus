use super::*;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const APIS: [&str; 3] = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];
const INTERNAL: &str = "dep-agentic-internal";
const MODELS: &str = "dep-agentic-models";
const PRIVATE_GATEWAY: &str = "agentic-internal";
const POD: &str = "agentic-api-pod";
const PRIVATE_POD: &str = "private-gateway-pod";
const SECRET: &str = "agentic-db";
const CA: &str = "thalamus-agentic-ca";
const CREATED: &str = "2025-01-01T00:00:00Z";
const POD_CREATED: &str = "2025-01-02T00:00:00Z";

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn image(c: char) -> String {
    format!("registry.example/image@{}", digest(c))
}

fn hash(deployment: &str) -> String {
    let value = format!(
        "{:x}",
        Sha256::digest(format!("svc/assistant/{deployment}"))
    );
    value[..32].to_owned()
}

fn meta(namespace: &str, name: &str, uid: &str) -> Value {
    json!({"namespace":namespace,"name":name,"uid":uid,"generation":1})
}

fn owner(kind: &str, name: &str, uid: &str) -> Value {
    let api_version = match kind {
        "Service" => "v1",
        "Gateway" => "gateway.networking.k8s.io/v1",
        _ => "apps/v1",
    };
    json!({"apiVersion":api_version,"kind":kind,"name":name,"uid":uid,"controller":true})
}

fn condition(name: &str) -> Value {
    json!({"type":name,"status":"True","observedGeneration":1,"reason":"Valid"})
}

fn ready_pod_status(image_id: &str, name: &str, ip: &str) -> Value {
    json!({"phase":"Running","podIP":ip,"podIPs":[{"ip":ip}],
        "conditions":[{"type":"Ready","status":"True"},{"type":"Initialized","status":"True"}],
        "containerStatuses":[{"name":name,"ready":true,"imageID":image_id}]})
}

fn annotations(team: &str, role: Option<&str>, per_deployment: bool) -> Value {
    let mut a = json!({"thalamus.ai/service":"svc","thalamus.ai/agentic-instance":"assistant",
        "thalamus.ai/team":team});
    if let Some(role) = role {
        a["thalamus.ai/agentic-role"] = json!(role);
    }
    if per_deployment {
        a["thalamus.ai/deployment"] = json!("dep");
        a["thalamus.ai/revision"] = json!("a".repeat(64));
    }
    a
}

fn http_route(
    name: &str,
    annotations: Value,
    gateway: &str,
    gateway_ns: &str,
    host: &str,
    rules: Value,
) -> Value {
    let mut metadata = meta("work", name, &format!("{name}-uid"));
    metadata["annotations"] = annotations;
    let parent = json!({"group":"gateway.networking.k8s.io","kind":"Gateway", "name":gateway,"namespace":gateway_ns});
    json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute","metadata":metadata,
        "spec":{"parentRefs":[parent.clone()],"hostnames":[host],"rules":rules},
        "status":{"parents":[{"parentRef":parent,"conditions":[condition("Accepted"),condition("ResolvedRefs")]}]}})
}

fn match_rule(method: &str, path: &str, backend: Value, rewrite: Option<&str>) -> Value {
    let mut rule = json!({"matches":[{"method":method,"path":{"type":"Exact","value":path}}],
        "backendRefs":[backend]});
    if let Some(path) = rewrite {
        rule["filters"] = json!([{"type":"URLRewrite","urlRewrite":{"path":{"type":"ReplaceFullPath","replaceFullPath":path}}}]);
    }
    rule
}

// This is the CEL emitted by the recipe, including both MCP URL shapes.
fn mcp_guard(candidate: bool) -> String {
    let prefix = if candidate { "/candidates/dep" } else { "" };
    let mut paths: Vec<String> = APIS.iter().map(|path| format!("{prefix}{path}")).collect();
    if !candidate {
        paths.push("/v1/responses/compact".into());
    }
    let tool = r##"type(t) == map && (!("type" in t) || t.type != "mcp" || ((!("server_url" in t) || t.server_url == null || (type(t.server_url) == string && allowed.exists(o, t.server_url == o || t.server_url.startsWith(o + "/") || t.server_url.startsWith(o + "?") || t.server_url.startsWith(o + "#")))) && (!("url" in t) || t.url == null || (type(t.url) == string && allowed.exists(o, t.url == o || t.url.startsWith(o + "/") || t.url.startsWith(o + "?") || t.url.startsWith(o + "#"))))))"##;
    let servers = r##"!("mcp_servers" in b) || b.mcp_servers == null || (type(b.mcp_servers) == list && b.mcp_servers.all(s, type(s) == map && "type" in s && s.type == "url" && "url" in s && type(s.url) == string && allowed.exists(o, s.url == o || s.url.startsWith(o + "/") || s.url.startsWith(o + "?") || s.url.startsWith(o + "#"))))"##;
    format!(
        r#"request.method != "POST" || !(request.path in {}) || [].with(allowed, json(request.body).with(b, type(b) == map && (!("tools" in b) || b.tools == null || (type(b.tools) == list && b.tools.all(t, {tool}))) && ({servers})))"#,
        serde_json::to_string(&paths).unwrap()
    )
}

fn complete_policy(p: &mut Value) {
    let expressions = p["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"]
        .as_array_mut()
        .unwrap();
    let job_scope = expressions
        .iter()
        .position(|e| e.as_str() == Some("apiKey.kind == \"job\" && apiKey.deployment == \"dep\""))
        .unwrap();
    let uuid = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
    expressions.insert(job_scope, json!(format!(r#"apiKey.kind != "debug" || (type(apiKey.debug_session_id) == string && apiKey.debug_session_id.matches("{uuid}") && type(apiKey.debug_model) == string && apiKey.debug_model == "svc" && apiKey.expiry != null && apiKey.budget != null && (request.method != "POST" || !(request.path in ["/v1/chat/completions", "/v1/completions", "/v1/responses", "/v1/embeddings", "/v1/messages", "/v1/messages/count_tokens"]) || json(request.body).model == apiKey.debug_model))"#)));
    expressions.insert(job_scope + 1, json!(r#"request.headers.all(h, (!h.startsWith("x-thalamus-") && !h.startsWith("x-llm-d-inference-")) || h in ["x-thalamus-user", "x-thalamus-project", "x-thalamus-team", "x-thalamus-class", "x-thalamus-backend", "x-llm-d-inference-objective", "x-llm-d-inference-fairness-id"])"#));
    p["spec"]["traffic"]["transformation"]["request"]["remove"] = json!([
        "x-api-key",
        "connection",
        "x-thalamus-backend",
        "x-thalamus-class",
        "x-llm-d-inference-objective",
        "x-llm-d-inference-fairness-id"
    ]);
    p["spec"]["traffic"]["rateLimit"]["global"]["backendRef"] =
        json!({"name":"ratelimit","namespace":"gateway","port":8081});
    p["spec"]["traffic"]["rateLimit"]["global"]["domain"] = json!("svc");
}

fn route_policy(
    route: &str,
    candidate: bool,
    shared: bool,
    outer: bool,
    models: bool,
    team: uuid::Uuid,
) -> Value {
    let mut p = tests::policy(route, false, team);
    complete_policy(&mut p);
    p["metadata"]["uid"] = json!(format!("{route}-policy-uid"));
    let expressions = p["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"]
        .as_array_mut()
        .unwrap();
    let scope = "apiKey.kind == \"job\" && apiKey.deployment == \"dep\"";
    let index = expressions
        .iter()
        .position(|v| v.as_str() == Some(scope))
        .unwrap();
    expressions.remove(index);
    if candidate {
        expressions.push(json!(scope));
    } else if !outer {
        expressions.push(json!(
            "apiKey.kind != \"job\" || apiKey.deployment == \"dep\""
        ));
    }
    if outer {
        expressions.push(json!(mcp_guard(candidate)));
        let response = if candidate {
            "/candidates/dep/v1/responses"
        } else {
            "/v1/responses"
        };
        if shared {
            expressions.push(json!(format!("request.path != {} || request.method != \"POST\" || json(request.body).with(b, type(b) == map && b.store == false && (!(\"previous_response_id\" in b) || b.previous_response_id == null) && (!(\"conversation\" in b) || b.conversation == null) && (!(\"conversation_id\" in b) || b.conversation_id == null))", serde_json::to_string(response).unwrap())));
        } else {
            expressions.push(json!(format!(
                "apiKey.team == {}",
                serde_json::to_string(&team.to_string()).unwrap()
            )));
            expressions.push(json!(format!(
                "request.method != \"GET\" || (request.path != {} && request.path != {})",
                serde_json::to_string(response).unwrap(),
                serde_json::to_string(&format!("{response}/")).unwrap()
            )));
        }
        p["spec"]["traffic"]["transformation"]["request"]["remove"] = json!([
            "x-api-key",
            "connection",
            "x-thalamus-backend",
            "x-thalamus-user",
            "x-thalamus-project",
            "x-thalamus-team",
            "x-thalamus-class",
            "x-llm-d-inference-objective",
            "x-llm-d-inference-fairness-id"
        ]);
        p["spec"]["traffic"]["transformation"]["request"]["set"] =
            json!([{"name":"authorization","value":"\"Bearer \" + apiKey.key.unredacted()"}]);
        p["spec"]["traffic"]
            .as_object_mut()
            .unwrap()
            .remove("rateLimit");
    } else if models {
        for descriptor in p["spec"]["traffic"]["rateLimit"]["global"]["descriptors"]
            .as_array_mut()
            .unwrap()
        {
            descriptor["unit"] = json!("Requests");
        }
    }
    p
}

fn endpoint_slice(
    service: &str,
    service_uid: &str,
    pod: &str,
    pod_uid: &str,
    ip: &str,
    port: u16,
) -> Value {
    let mut metadata = meta(
        "work",
        &format!("{service}-slice"),
        &format!("{service}-slice-uid"),
    );
    metadata["labels"] = json!({"kubernetes.io/service-name":service,
        "endpointslice.kubernetes.io/managed-by":"endpointslice-controller.k8s.io"});
    metadata["ownerReferences"] = json!([owner("Service", service, service_uid)]);
    json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":metadata,"addressType":"IPv4",
        "ports":[{"port":port,"protocol":"TCP"}],
        "endpoints":[{"addresses":[ip],"conditions":{"ready":true,"serving":true,"terminating":false},
            "targetRef":{"kind":"Pod","namespace":"work","name":pod,"uid":pod_uid}}]})
}

fn named<'a>(values: &'a mut [Value], name: &str) -> &'a mut Value {
    values
        .iter_mut()
        .find(|v| v.pointer("/metadata/name").and_then(Value::as_str) == Some(name))
        .unwrap()
}

struct Fixture {
    config: RuntimeConfig,
    resources: LiveResources,
    route: String,
    direct: RouteSnapshot,
    at: DateTime<Utc>,
    workload: String,
    backend: String,
}

impl Fixture {
    fn new(candidate: bool, shared: bool) -> Self {
        let (mut config, mut r, direct_name) = tests::fixture(false);
        complete_policy(&mut r.policies[0]);
        let team_id = config.routes["svc"].team_id;
        let team = if shared {
            "shared".to_owned()
        } else {
            team_id.to_string()
        };
        config.trusted_agentic_images = vec![image('9')];
        // Agentic APIs are served by the same observed AI backend and pool.
        r.backends[0]["spec"]["ai"]["provider"]["custom"]["formats"] = json!([
            {"type":"Completions","path":"/v1/chat/completions"},
            {"type":"Responses","path":"/v1/responses"},
            {"type":"Messages","path":"/v1/messages"}]);
        r.backends[0]["spec"]["policies"]["ai"] = json!({"routes":{"/v1/chat/completions":"Completions","/v1/responses":"Responses","/v1/messages":"Messages"}});
        r.gateways[0]["spec"]["listeners"][0]["tls"] =
            json!({"mode":"Terminate","certificateRefs":[{"kind":"Secret","name":"public-tls"}]});
        let direct_name = if candidate {
            direct_name
        } else {
            r.routes[0]["metadata"]["name"] = json!("svc");
            r.routes[0]["spec"]["rules"][0]["matches"][0]["path"]["value"] =
                json!("/v1/chat/completions");
            r.routes[0]["spec"]["rules"][0]
                .as_object_mut()
                .unwrap()
                .remove("filters");
            r.policies[0]["metadata"]["name"] = json!("svc-auth");
            r.policies[0]["spec"]["targetRefs"][0]["name"] = json!("svc");
            let expressions =
                r.policies[0]["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"]
                    .as_array_mut()
                    .unwrap();
            expressions.retain(|v| {
                v.as_str() != Some("apiKey.kind == \"job\" && apiKey.deployment == \"dep\"")
            });
            "svc".into()
        };
        let workload = format!("agentic-pod-{}", hash("dep"));
        let backend = format!("agentic-backend-{}", hash("dep"));
        let public_name = "svc-assistant";
        let candidate_name = format!("agentic-cand-{}", hash("dep"));
        // In a candidate observation the applicable public route still serves the
        // prior stable deployment; it is used for discovery, not as evidence of dep.
        let public_backend = if candidate {
            format!("agentic-backend-{}", hash("old"))
        } else {
            backend.clone()
        };
        let backend_ref = |name: &str| json!({"group":"agentgateway.dev","kind":"AgentgatewayBackend","name":name,"weight":1});
        let mut matches: Vec<Value> = APIS
            .iter()
            .map(|path| json!({"method":"POST","path":{"type":"Exact","value":path}}))
            .collect();
        matches.push(json!({"method":"GET","path":{"type":"PathPrefix","value":"/v1/models"}}));
        if !shared {
            matches.extend([
                json!({"method":"POST","path":{"type":"Exact","value":"/v1/responses/compact"}}),
                json!({"method":"GET","path":{"type":"PathPrefix","value":"/v1/responses"}}),
                json!({"method":"DELETE","path":{"type":"PathPrefix","value":"/v1/responses"}}),
                json!({"method":"POST","path":{"type":"PathPrefix","value":"/v1/conversations"}}),
                json!({"method":"GET","path":{"type":"PathPrefix","value":"/v1/conversations"}}),
                json!({"method":"DELETE","path":{"type":"PathPrefix","value":"/v1/conversations"}}),
            ]);
        }
        r.routes.push(http_route(
            public_name,
            annotations(&team, Some("public"), false),
            "gw",
            "gateway",
            "assistant.example",
            json!([{"matches":matches,"backendRefs":[backend_ref(&public_backend)]}]),
        ));
        r.policies.push(route_policy(
            public_name,
            false,
            shared,
            true,
            false,
            team_id,
        ));
        if candidate {
            let rules: Vec<_> = APIS
                .iter()
                .map(|path| {
                    match_rule(
                        "POST",
                        &format!("/candidates/dep{path}"),
                        backend_ref(&backend),
                        Some(path),
                    )
                })
                .collect();
            r.routes.push(http_route(
                &candidate_name,
                annotations(&team, Some("candidate"), true),
                "gw",
                "gateway",
                "assistant.example",
                json!(rules),
            ));
            r.policies.push(route_policy(
                &candidate_name,
                true,
                shared,
                true,
                false,
                team_id,
            ));
        }
        let route = if candidate {
            candidate_name
        } else {
            public_name.into()
        };
        let mut backend_meta = meta("work", &backend, "static-backend-uid");
        backend_meta["annotations"] = annotations(&team, None, true);
        r.backends.push(json!({"apiVersion":"agentgateway.dev/v1alpha1","kind":"AgentgatewayBackend","metadata":backend_meta,
            "spec":{"static":{"host":format!("{workload}.work.svc.cluster.local"),"port":9000},
                "policies":{"transformation":{"response":{"set":[{"name":"x-thalamus-backend","value":serde_json::to_string(&format!("svc/dep/{}", "a".repeat(64))).unwrap()}]}}}}}));
        let mut service_meta = meta("work", &workload, "agentic-service-uid");
        service_meta["annotations"] = annotations(&team, None, true);
        let labels = json!({"thalamus.dev/agentic-instance":workload});
        r.agentic_services.push(json!({"apiVersion":"v1","kind":"Service","metadata":service_meta,
            "spec":{"type":"ClusterIP","clusterIP":"10.96.2.1","selector":labels,"ports":[{"port":9000,"targetPort":9000,"protocol":"TCP"}]}}));
        let pod_spec = json!({"automountServiceAccountToken":false,
            "volumes":[{"name":"internal-ca","configMap":{"name":CA,"items":[{"key":"ca.crt","path":"ca.crt"}]}}],
            "containers":[{"name":"agentic-api","image":image('9'),"ports":[{"containerPort":9000}],
                "env":[{"name":"LLM_API_BASE","value":"https://agentic-internal.work.svc.cluster.local/agentic/svc/dep"},
                    {"name":"SSL_CERT_FILE","value":"/var/run/agentic-ca/ca.crt"},
                    {"name":"DATABASE_URL","valueFrom":{"secretKeyRef":{"name":SECRET,"key":"DATABASE_URL"}}},
                    {"name":"SKIP_LLM_READY_CHECK","value":"true"},{"name":"AGENTIC_MCP_ALLOWED_HOSTS","value":""}],
                "volumeMounts":[{"name":"internal-ca","mountPath":"/var/run/agentic-ca","readOnly":true}],
                "securityContext":{"runAsNonRoot":true,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}},
                "startupProbe":{"httpGet":{"path":"/health","port":9000},"periodSeconds":5,"failureThreshold":60},
                "readinessProbe":{"httpGet":{"path":"/ready","port":9000},"periodSeconds":5,"failureThreshold":30},
                "livenessProbe":{"httpGet":{"path":"/health","port":9000},"periodSeconds":10}}]});
        let mut deployment_meta = meta("work", &workload, "agentic-deployment-uid");
        deployment_meta["annotations"] = annotations(&team, None, true);
        r.agentic_deployments.push(json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":deployment_meta,
            "spec":{"replicas":1,"strategy":{"type":"RollingUpdate"},"selector":{"matchLabels":labels},
                "template":{"metadata":{"labels":labels,"annotations":annotations(&team, None, true)},"spec":pod_spec}},
            "status":{"observedGeneration":1,"replicas":1,"updatedReplicas":1,"readyReplicas":1,"availableReplicas":1}}));
        let mut pod_meta = meta("work", POD, "agentic-pod-uid");
        pod_meta["creationTimestamp"] = json!(POD_CREATED);
        pod_meta["labels"] = labels;
        pod_meta["ownerReferences"] = json!([owner("ReplicaSet", "agentic-rs", "agentic-rs-uid")]);
        r.pods.push(
            json!({"apiVersion":"v1","kind":"Pod","metadata":pod_meta,"spec":pod_spec,
            "status":ready_pod_status(&digest('9'), "agentic-api", "10.1.2.3")}),
        );
        let mut rs_meta = meta("work", "agentic-rs", "agentic-rs-uid");
        rs_meta["ownerReferences"] =
            json!([owner("Deployment", &workload, "agentic-deployment-uid")]);
        r.workloads
            .push(json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":rs_meta}));
        r.agentic_endpoint_slices.push(endpoint_slice(
            &workload,
            "agentic-service-uid",
            POD,
            "agentic-pod-uid",
            "10.1.2.3",
            9000,
        ));
        let mut ca_meta = meta("work", CA, "ca-uid");
        ca_meta["creationTimestamp"] = json!(CREATED);
        r.config_maps.push(json!({"apiVersion":"v1","kind":"ConfigMap","metadata":ca_meta,"immutable":true,
            "data":{"ca.crt":include_str!("../../thalamus-access/tests/fixtures/oidc-test-ca.pem")}}));
        // This is deliberately the metadata projection, never the Secret's data.
        r.agentic_secret_metadata.push(json!({"apiVersion":"v1","kind":"Secret","immutable":true,
            "metadata":{"namespace":"work","name":SECRET,"uid":"db-secret-uid","resourceVersion":"7","creationTimestamp":CREATED}}));
        let private_host = "agentic-internal.work.svc.cluster.local";
        r.gateways.push(json!({"apiVersion":"gateway.networking.k8s.io/v1","kind":"Gateway","metadata":meta("work", PRIVATE_GATEWAY, "private-gateway-uid"),
            "spec":{"listeners":[{"name":"https","hostname":private_host,"protocol":"HTTPS","port":443,
                "tls":{"mode":"Terminate","certificateRefs":[{"kind":"Secret","name":"private-tls"}]}}]},
            "status":{"conditions":[condition("Accepted"),condition("Programmed")],"listeners":[{"name":"https","conditions":[condition("Accepted"),condition("Programmed")]}]}}));
        r.agentic_services.push(json!({"apiVersion":"v1","kind":"Service","metadata":meta("work", PRIVATE_GATEWAY, "private-service-uid"),
            "spec":{"type":"ClusterIP","clusterIP":"10.96.2.2","selector":{"gateway.networking.k8s.io/gateway-name":PRIVATE_GATEWAY},
                "ports":[{"port":443,"targetPort":443,"protocol":"TCP"}]}}));
        let mut gateway_pod_meta = meta("work", PRIVATE_POD, "private-pod-uid");
        gateway_pod_meta["labels"] =
            json!({"gateway.networking.k8s.io/gateway-name":PRIVATE_GATEWAY});
        gateway_pod_meta["ownerReferences"] =
            json!([owner("ReplicaSet", "private-rs", "private-rs-uid")]);
        r.pods.push(json!({"apiVersion":"v1","kind":"Pod","metadata":gateway_pod_meta,
            "spec":{"containers":[{"name":"gateway","image":image('8'),"ports":[{"containerPort":443}]}]},
            "status":ready_pod_status(&digest('8'), "gateway", "10.1.2.4")}));
        let mut private_rs = meta("work", "private-rs", "private-rs-uid");
        private_rs["ownerReferences"] = json!([owner(
            "Deployment",
            "private-deployment",
            "private-deployment-uid"
        )]);
        r.workloads
            .push(json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":private_rs}));
        let mut private_deployment = meta("work", "private-deployment", "private-deployment-uid");
        private_deployment["ownerReferences"] =
            json!([owner("Gateway", PRIVATE_GATEWAY, "private-gateway-uid")]);
        r.workloads.push(json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":private_deployment,
            "spec":{"replicas":1},"status":{"observedGeneration":1,"replicas":1,"updatedReplicas":1,"readyReplicas":1,"availableReplicas":1}}));
        r.agentic_endpoint_slices.push(endpoint_slice(
            PRIVATE_GATEWAY,
            "private-service-uid",
            PRIVATE_POD,
            "private-pod-uid",
            "10.1.2.4",
            443,
        ));
        let inner_annotations = json!({"thalamus.ai/service":"svc","thalamus.ai/deployment":"dep","thalamus.ai/revision":"a".repeat(64)});
        let rules: Vec<_> = APIS
            .iter()
            .map(|path| {
                match_rule(
                    "POST",
                    &format!("/agentic/svc/dep{path}"),
                    backend_ref("dep-backend"),
                    Some(path),
                )
            })
            .collect();
        r.routes.push(http_route(
            INTERNAL,
            inner_annotations.clone(),
            PRIVATE_GATEWAY,
            "work",
            private_host,
            json!(rules),
        ));
        r.policies.push(route_policy(
            INTERNAL, candidate, shared, false, false, team_id,
        ));
        let pool = json!({"group":"inference.networking.k8s.io","kind":"InferencePool","name":"dep-inference-pool"});
        r.routes.push(http_route(
            MODELS,
            inner_annotations,
            PRIVATE_GATEWAY,
            "work",
            private_host,
            json!([match_rule(
                "GET",
                "/agentic/svc/dep/v1/models",
                pool,
                Some("/v1/models")
            )]),
        ));
        r.policies.push(route_policy(
            MODELS, candidate, shared, false, true, team_id,
        ));
        let at = Utc::now();
        let direct = join_route(&config, "svc", &direct_name, &r, at)
            .expect("complete direct model observation");
        let fixture = Self {
            config,
            resources: r,
            route,
            direct,
            at,
            workload,
            backend,
        };
        fixture
            .observe()
            .expect("complete agentic fixture must be independently observable");
        fixture
    }

    fn observe(&self) -> Result<RouteSnapshot> {
        join_agentic_route(
            &self.config,
            "svc",
            &self.route,
            &self.direct,
            &self.resources,
            self.at,
        )
    }

    fn route_mut(&mut self, name: &str) -> &mut Value {
        named(&mut self.resources.routes, name)
    }
    fn pod_mut(&mut self, name: &str) -> &mut Value {
        named(&mut self.resources.pods, name)
    }
    fn policy_mut(&mut self, route: &str) -> &mut Value {
        named(&mut self.resources.policies, &format!("{route}-auth"))
    }

    // Give a synthetic replacement its own Pod/ReplicaSet identities and
    // current Deployment status, just as a completed rollout would.
    fn replace_agentic_pod(&mut self) {
        self.resources.agentic_deployments[0]["metadata"]["generation"] = json!(2);
        self.resources.agentic_deployments[0]["status"]["observedGeneration"] = json!(2);
        let rs = named(&mut self.resources.workloads, "agentic-rs");
        rs["metadata"]["name"] = json!("agentic-rs-2");
        rs["metadata"]["uid"] = json!("agentic-rs-uid-2");
        let pod = self.pod_mut(POD);
        pod["metadata"]["uid"] = json!("agentic-pod-uid-2");
        pod["metadata"]["creationTimestamp"] = json!("2025-02-02T00:00:00Z");
        pod["metadata"]["ownerReferences"][0]["name"] = json!("agentic-rs-2");
        pod["metadata"]["ownerReferences"][0]["uid"] = json!("agentic-rs-uid-2");
        self.resources.agentic_endpoint_slices[0]["endpoints"][0]["targetRef"]["uid"] =
            json!("agentic-pod-uid-2");
    }
}

#[track_caller]
fn invalid(result: Result<RouteSnapshot>) {
    match result {
        Err(Error::Invalid(_)) => {}
        Err(other) => panic!("expected invalid runtime evidence, got {other}"),
        Ok(snapshot) => panic!("expected invalid runtime evidence, got {snapshot:?}"),
    }
}

#[test]
fn candidate_and_public_join_complete_live_chain_and_compare_promotion() {
    let candidate = Fixture::new(true, false);
    let public = Fixture::new(false, false);
    let c = candidate.observe().unwrap();
    let p = public.observe().unwrap();
    assert!(c.agentic && p.agentic);
    assert_eq!(c.route, candidate.route);
    assert_eq!(p.route, "svc-assistant");
    assert_eq!(c.hostnames, vec!["assistant.example"]);
    assert_eq!(c.ports, vec![443]);
    assert_eq!(
        c.listeners,
        vec![RouteListener {
            hostname: "assistant.example".into(),
            port: 443,
            protocol: "HTTPS".into()
        }]
    );
    assert_eq!(
        c.paths,
        vec![
            "/candidates/dep/v1/chat/completions",
            "/candidates/dep/v1/messages",
            "/candidates/dep/v1/responses"
        ]
    );
    assert_eq!(c.weights, candidate.direct.weights);
    assert_eq!(p.weights, public.direct.weights);
    assert_eq!(c.deployments.len(), 1);
    let deployment = &c.deployments[0];
    assert_eq!(deployment.deployment, "dep");
    assert_eq!(deployment.revision, "a".repeat(64));
    assert_eq!(deployment.backend, format!("svc/dep/{}", "a".repeat(64)));
    assert_eq!(deployment.runtime.model_content, digest('d'));
    assert_eq!(
        deployment.runtime.image_digests.get("container/main"),
        Some(&digest('b'))
    );
    assert_eq!(
        deployment
            .runtime
            .component_versions
            .get(&format!("agentic/{}/image", candidate.workload)),
        Some(&digest('9'))
    );
    assert_eq!(
        deployment
            .runtime
            .component_versions
            .get("agentic-pod-uid/agentic-pod-uid"),
        Some(&digest('9'))
    );
    assert_eq!(
        deployment
            .runtime
            .component_versions
            .get("gateway/work/agentic-internal/container/gateway"),
        Some(&digest('8'))
    );
    assert_ne!(
        deployment.runtime.effective_configuration,
        candidate.direct.deployments[0]
            .runtime
            .effective_configuration
    );
    assert_ne!(
        deployment.desired.resolved_dependencies,
        candidate.direct.deployments[0]
            .desired
            .resolved_dependencies
    );
    let underlying = deployment
        .underlying_model
        .as_ref()
        .expect("agentic evidence retains its independently observed model");
    assert_eq!(
        underlying.rollout_configuration,
        candidate.direct.deployments[0].rollout_configuration
    );
    assert_eq!(underlying.runtime, candidate.direct.deployments[0].runtime);
    assert!(!deployment.same_serving_revision(&candidate.direct.deployments[0]));
    assert!(deployment.same_serving_revision(&p.deployments[0]));
    assert!(!c.same_identity(&p));
    let mut later = c.clone();
    later.observed_at += chrono::Duration::seconds(1);
    assert!(c.same_identity(&later));
}

#[test]
fn shared_stateless_public_and_candidate_chains_are_observed() {
    let candidate = Fixture::new(true, true).observe().unwrap();
    let public = Fixture::new(false, true).observe().unwrap();
    assert_eq!(
        public.paths,
        vec![
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/models",
            "/v1/responses"
        ]
    );
    assert!(candidate.deployments[0].same_serving_revision(&public.deployments[0]));
}

#[test]
fn wrong_outer_weight_or_old_target_is_rejected_for_public_and_candidate() {
    for candidate in [false, true] {
        let mut f = Fixture::new(candidate, false);
        let route = f.route.clone();
        f.route_mut(&route)["spec"]["rules"][0]["backendRefs"][0]["weight"] = json!(2);
        invalid(f.observe());
        let mut f = Fixture::new(candidate, false);
        let route = f.route.clone();
        f.route_mut(&route)["spec"]["rules"][0]["backendRefs"][0]["name"] =
            json!(format!("agentic-backend-{}", hash("old")));
        invalid(f.observe());
    }
}

#[test]
fn endpoint_slice_must_identify_the_ready_owned_pod_and_actual_address() {
    let mut f = Fixture::new(true, false);
    f.resources.agentic_endpoint_slices[0]["endpoints"][0]["targetRef"]["uid"] =
        json!("foreign-pod-uid");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.resources.agentic_endpoint_slices[0]["endpoints"][0]["addresses"][0] = json!("10.1.2.99");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.resources.agentic_endpoint_slices[0]["ports"][0]["port"] = json!(9999);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.resources.agentic_endpoint_slices[0]["metadata"]["ownerReferences"][0]["uid"] =
        json!("foreign-service-uid");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.pod_mut(POD)["status"]["conditions"][0]["status"] = json!("False");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.resources.agentic_endpoint_slices[1]["endpoints"][0]["targetRef"]["uid"] =
        json!("foreign-gateway-pod");
    invalid(f.observe());
}

#[test]
fn secret_metadata_is_immutable_old_and_part_of_observed_identity() {
    let mut f = Fixture::new(true, false);
    f.resources.agentic_secret_metadata[0]["immutable"] = json!(false);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.resources.agentic_secret_metadata[0]["metadata"]["creationTimestamp"] = json!(POD_CREATED);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    f.resources.agentic_secret_metadata[0]["metadata"]["uid"] = json!("replacement-secret-uid");
    f.resources.agentic_secret_metadata[0]["metadata"]["resourceVersion"] = json!("8");
    f.resources.agentic_secret_metadata[0]["metadata"]["creationTimestamp"] =
        json!("2025-02-01T00:00:00Z");
    f.replace_agentic_pod();
    let after = f.observe().unwrap();
    assert_ne!(
        before.deployments[0].desired.resolved_dependencies,
        after.deployments[0].desired.resolved_dependencies
    );
    assert_ne!(
        before.deployments[0].runtime.effective_configuration,
        after.deployments[0].runtime.effective_configuration
    );
    assert!(!before.same_identity(&after));
    assert!(!before.deployments[0].same_serving_revision(&after.deployments[0]));
    assert!(f.resources.agentic_secret_metadata[0].get("data").is_none());
    assert!(
        f.resources.agentic_secret_metadata[0]
            .get("stringData")
            .is_none()
    );
}

#[test]
fn missing_referenced_secret_or_unreviewed_secret_environment_is_rejected() {
    let mut f = Fixture::new(true, false);
    f.resources.agentic_secret_metadata.clear();
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    let extra = json!({"name":"UNREVIEWED_TOKEN","valueFrom":{"secretKeyRef":{"name":"agentic-db","key":"DATABASE_URL"}}});
    f.pod_mut(POD)["spec"]["containers"][0]["env"]
        .as_array_mut()
        .unwrap()
        .push(extra.clone());
    f.resources.agentic_deployments[0]["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array_mut()
        .unwrap()
        .push(extra);
    invalid(f.observe());
}

#[test]
fn trust_policy_and_actual_image_id_are_independent_evidence() {
    let mut f = Fixture::new(true, false);
    f.config.trusted_agentic_images.clear();
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.pod_mut(POD)["status"]["containerStatuses"][0]["imageID"] = json!(digest('7'));
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    f.config.trusted_agentic_images.push(image('7'));
    assert!(
        before.same_identity(&f.observe().unwrap()),
        "trust options alone are not runtime identity"
    );
    f.resources.agentic_deployments[0]["spec"]["template"]["spec"]["containers"][0]["image"] =
        json!(image('7'));
    f.pod_mut(POD)["spec"]["containers"][0]["image"] = json!(image('7'));
    f.pod_mut(POD)["status"]["containerStatuses"][0]["imageID"] = json!(digest('7'));
    f.replace_agentic_pod();
    let after = f.observe().unwrap();
    assert_eq!(
        after.deployments[0]
            .runtime
            .component_versions
            .get(&format!("agentic/{}/image", f.workload)),
        Some(&digest('7'))
    );
    assert!(!before.same_identity(&after));
    assert!(!before.deployments[0].same_serving_revision(&after.deployments[0]));
}

#[test]
fn changed_effective_agentic_pod_config_is_not_the_same_observation() {
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    f.resources.agentic_deployments[0]["spec"]["template"]["spec"]["terminationGracePeriodSeconds"] =
        json!(45);
    f.pod_mut(POD)["spec"]["terminationGracePeriodSeconds"] = json!(45);
    f.replace_agentic_pod();
    let after = f.observe().unwrap();
    assert_ne!(
        before.deployments[0].runtime.effective_configuration,
        after.deployments[0].runtime.effective_configuration
    );
    assert!(!before.same_identity(&after));
    assert!(!before.deployments[0].same_serving_revision(&after.deployments[0]));
    let mut f = Fixture::new(true, false);
    f.pod_mut(POD)["spec"]["containers"][0]["env"][0]["value"] =
        json!("https://old.work.svc.cluster.local/agentic/svc/dep");
    invalid(f.observe());
}

#[test]
fn replacing_an_unchanged_pod_changes_full_evidence_but_not_serving_revision() {
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    f.replace_agentic_pod();
    let after = f.observe().unwrap();
    assert!(!before.same_identity(&after));
    assert_ne!(
        before.deployments[0].desired.resolved_dependencies,
        after.deployments[0].desired.resolved_dependencies
    );
    assert_eq!(
        before.deployments[0].runtime.effective_configuration,
        after.deployments[0].runtime.effective_configuration
    );
    assert!(before.deployments[0].same_serving_revision(&after.deployments[0]));
}

#[test]
fn auth_policies_and_candidate_job_binding_are_required_on_both_hops() {
    for candidate in [false, true] {
        let mut f = Fixture::new(candidate, false);
        let outer = f.route.clone();
        f.resources.policies.retain(|p| {
            p.pointer("/spec/targetRefs/0/name").and_then(Value::as_str) != Some(outer.as_str())
        });
        invalid(f.observe());
        let mut f = Fixture::new(candidate, false);
        f.policy_mut(INTERNAL)["spec"]["targetRefs"][0]["name"] = json!("foreign-route");
        invalid(f.observe());
    }
    for route in ["outer", INTERNAL, MODELS] {
        let mut f = Fixture::new(true, false);
        let name = if route == "outer" {
            f.route.clone()
        } else {
            route.into()
        };
        let expressions =
            f.policy_mut(&name)["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"]
                .as_array_mut()
                .unwrap();
        expressions.retain(|e| {
            e.as_str() != Some("apiKey.kind == \"job\" && apiKey.deployment == \"dep\"")
        });
        invalid(f.observe());
        let mut f = Fixture::new(true, false);
        let name = if route == "outer" {
            f.route.clone()
        } else {
            route.into()
        };
        let expressions =
            f.policy_mut(&name)["spec"]["traffic"]["authorization"]["policy"]["matchExpressions"]
                .as_array_mut()
                .unwrap();
        let scope = expressions
            .iter_mut()
            .find(|e| e.as_str() == Some("apiKey.kind == \"job\" && apiKey.deployment == \"dep\""))
            .unwrap();
        *scope = json!("apiKey.kind == \"job\" && apiKey.deployment == \"old\"");
        invalid(f.observe());
    }
}

#[test]
fn private_inference_and_models_routes_cannot_redirect_to_old_targets() {
    let mut f = Fixture::new(true, false);
    f.route_mut(INTERNAL)["spec"]["rules"][0]["backendRefs"][0]["name"] = json!("old-backend");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.route_mut(MODELS)["spec"]["rules"][0]["backendRefs"][0]["name"] = json!("old-inference-pool");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.route_mut(INTERNAL)["spec"]["rules"][0]["filters"][0]["urlRewrite"]["path"]["replaceFullPath"] =
        json!("/v1/responses");
    invalid(f.observe());
}

#[test]
fn private_gateway_requires_https_and_an_older_immutable_ca() {
    let mut f = Fixture::new(true, false);
    named(&mut f.resources.gateways, PRIVATE_GATEWAY)["spec"]["listeners"][0]["protocol"] =
        json!("HTTP");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    named(&mut f.resources.gateways, PRIVATE_GATEWAY)["spec"]["listeners"][0]
        .as_object_mut()
        .unwrap()
        .remove("tls");
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    named(&mut f.resources.config_maps, CA)["immutable"] = json!(false);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    named(&mut f.resources.config_maps, CA)["metadata"]["creationTimestamp"] = json!(POD_CREATED);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    named(&mut f.resources.config_maps, CA)["metadata"]["uid"] = json!("replacement-ca-uid");
    named(&mut f.resources.config_maps, CA)["metadata"]["creationTimestamp"] =
        json!("2025-02-01T00:00:00Z");
    f.replace_agentic_pod();
    let after = f.observe().unwrap();
    assert_ne!(
        before.deployments[0].desired.resolved_dependencies,
        after.deployments[0].desired.resolved_dependencies
    );
    assert_ne!(
        before.deployments[0].runtime.effective_configuration,
        after.deployments[0].runtime.effective_configuration
    );
    assert!(!before.same_identity(&after));
    assert!(!before.deployments[0].same_serving_revision(&after.deployments[0]));
}

#[test]
fn competing_route_and_forged_static_backend_header_are_rejected() {
    let mut f = Fixture::new(true, false);
    let mut competing = named(&mut f.resources.routes, &f.route).clone();
    competing["metadata"]["name"] = json!("foreign");
    competing["metadata"]["namespace"] = json!("another");
    competing["metadata"]["annotations"] = json!({});
    f.resources.routes.push(competing);
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    let backend = f.backend.clone();
    named(&mut f.resources.backends, &backend)["spec"]["policies"]["transformation"]["response"]
        ["set"][0]["value"] = json!("\"svc/old/forged\"");
    invalid(f.observe());
}

#[test]
fn configured_trust_or_a_forged_direct_snapshot_cannot_substitute_for_live_hops() {
    let mut f = Fixture::new(true, false);
    let before = f.observe().unwrap();
    f.resources.agentic_services.retain(|s| {
        s.pointer("/metadata/name").and_then(Value::as_str) != Some(f.workload.as_str())
    });
    let direct_again = join_route(&f.config, "svc", &f.direct.route, &f.resources, f.at).unwrap();
    assert!(f.direct.same_identity(&direct_again));
    invalid(f.observe());
    let mut f = Fixture::new(true, false);
    f.direct.deployments[0].revision = "f".repeat(64);
    invalid(f.observe());
    assert!(before.agentic);
}
