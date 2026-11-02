//! Exercise the catalog status boundary through actual kube GET and LIST requests.
use super::*;
use axum::{
    extract::State,
    http::{StatusCode, Uri},
    Json, Router,
};
use serde_json::json;
use std::collections::BTreeMap;
use thalamus_core::{PublishedService, ServiceDeployment};
use thalamus_runtime::ServiceRoutes;
use tokio::task::JoinHandle;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_COMMIT: &str = "abcdef0123456789abcdef0123456789abcdef01";
const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMAGE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PICKER_DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const NS: &str = "inference";
const MODEL: &str = "model-a";
const POOL: &str = "model-a-inference-pool";
const PICKER: &str = "model-a-epp";
const MODEL_UID: &str = "11111111-1111-4111-8111-111111111111";
const MODEL_RS_UID: &str = "22222222-2222-4222-8222-222222222222";
const PICKER_UID: &str = "33333333-3333-4333-8333-333333333333";
const PICKER_DEPLOYMENT_UID: &str = "44444444-4444-4444-8444-444444444444";
const PICKER_RS_UID: &str = "55555555-5555-4555-8555-555555555555";
const PICKER_POD_UID: &str = "66666666-6666-4666-8666-666666666666";
const FILLER_UID: &str = "99999999-9999-4999-8999-999999999999";
const FILLER_RS_UID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MODEL_PATH: &str = "/apis/apps/v1/namespaces/inference/deployments/model-a";
const MODEL_RS_PATH: &str = "/apis/apps/v1/namespaces/inference/replicasets/model-a-6dfb6";
const FILLER_PATH: &str = "/apis/apps/v1/namespaces/inference/deployments/model-a-filler";
const FILLER_RS_PATH: &str = "/apis/apps/v1/namespaces/inference/replicasets/model-a-filler-6dfb6";
const BACKEND_PATH: &str =
    "/apis/agentgateway.dev/v1alpha1/namespaces/inference/agentgatewaybackends/model-a-backend";
const POOL_PATH: &str = "/apis/inference.networking.k8s.io/v1/namespaces/inference/inferencepools/model-a-inference-pool";
const ROUTE_PATH: &str =
    "/apis/gateway.networking.k8s.io/v1/namespaces/inference/httproutes/model-a-benchmark";
const TRAFFIC_ROUTE_PATH: &str =
    "/apis/gateway.networking.k8s.io/v1/namespaces/inference/httproutes/example";
const CANDIDATE_ROUTE_PATH: &str =
    "/apis/gateway.networking.k8s.io/v1/namespaces/inference/httproutes/model-a-candidate";
const KSERVE_PATH: &str =
    "/apis/serving.kserve.io/v1alpha2/namespaces/inference/llminferenceservices/model-a";
const KSERVE_DEPLOYMENT_PATH: &str =
    "/apis/apps/v1/namespaces/inference/deployments/model-a-kserve-workload";
const KSERVE_SERVICE_PATH: &str =
    "/api/v1/namespaces/inference/services/model-a-kserve-workload-svc";
const KSERVE_PICKER_SERVICE_NAME: &str = "model-a-epp-service";
const KSERVE_PICKER_DEPLOYMENT_NAME: &str = "model-a-kserve-router-scheduler";
const KSERVE_PICKER_RS_NAME: &str = "model-a-kserve-router-scheduler-6dfb6";
const KSERVE_PICKER_POD_NAME: &str = "model-a-kserve-router-scheduler-6dfb6-abcde";
const KSERVE_PICKER_SERVICE_PATH: &str =
    "/api/v1/namespaces/inference/services/model-a-epp-service";
const KSERVE_PICKER_DEPLOYMENT_PATH: &str =
    "/apis/apps/v1/namespaces/inference/deployments/model-a-kserve-router-scheduler";
const KSERVE_PICKER_RS_PATH: &str =
    "/apis/apps/v1/namespaces/inference/replicasets/model-a-kserve-router-scheduler-6dfb6";
const PICKER_SERVICE_PATH: &str = "/api/v1/namespaces/inference/services/model-a-epp";
const PICKER_DEPLOYMENT_PATH: &str = "/apis/apps/v1/namespaces/inference/deployments/model-a-epp";
const PICKER_RS_PATH: &str = "/apis/apps/v1/namespaces/inference/replicasets/model-a-epp-6dfb6";
const PODS_PATH: &str = "/api/v1/namespaces/inference/pods";
const SLICES_PATH: &str = "/apis/discovery.k8s.io/v1/namespaces/inference/endpointslices";

fn owner(api_version: &str, kind: &str, name: &str, uid: &str) -> Value {
    json!({"apiVersion":api_version,"kind":kind,"name":name,"uid":uid,
        "controller":true,"blockOwnerDeletion":true})
}

fn catalog() -> CatalogService {
    CatalogService {
        name: "example".into(),
        namespace: NS.into(),
        pool: Some("gpu-pool".into()),
        deployments: vec![CatalogDeployment {
            routing: ServiceDeployment {
                deployment: MODEL.into(),
                weight: 1,
            },
            artifact: "weights".into(),
            hardware: "gpu".into(),
            image: Some(format!("registry.example/vllm@sha256:{IMAGE_DIGEST}")),
            revision: Some(REVISION.into()),
            role: None,
            replicas: 1,
            packaged_model: true,
            rollout_strategy: "RollingUpdate".into(),
            pooled: true,
        }],
        source: Default::default(),
    }
}

fn runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        namespace: NS.into(),
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
            "example".into(),
            ServiceRoutes {
                public: "example".into(),
                team_id: Uuid::from_u128(1),
                project_id: Uuid::from_u128(2),
                service: PublishedService {
                    name: "example".into(),
                    protocols: Default::default(),
                    deployments: vec![ServiceDeployment {
                        deployment: MODEL.into(),
                        weight: 1,
                    }],
                    access_policy: "example-access".into(),
                    owners: vec![],
                },
            },
        )]),
    }
}

struct Fixture {
    objects: BTreeMap<String, Value>,
    pods: Vec<Value>,
    // None means the LIST itself failed; an empty list is a successful observation.
    slices: Option<Vec<Value>>,
}

impl Fixture {
    fn ready() -> Self {
        let image = format!("registry.example/vllm@sha256:{IMAGE_DIGEST}");
        let applied = format!("main@sha1:{COMMIT}");
        let flux_labels = json!({"kustomize.toolkit.fluxcd.io/name":"serving",
            "kustomize.toolkit.fluxcd.io/namespace":"flux-system"});
        let model_labels = json!({"thalamus.ai/deployment":MODEL,"thalamus.ai/pool-role":"normal"});
        let model_pod_labels = json!({"thalamus.ai/deployment":MODEL,
            "thalamus.ai/pool-role":"normal","pod-template-hash":"6dfb6"});
        let picker_labels = json!({"app.kubernetes.io/name":PICKER});
        let picker_pod_labels =
            json!({"app.kubernetes.io/name":PICKER,"pod-template-hash":"6dfb6"});
        let model_container = json!({"name":"main","image":image,
            "command":["vllm","serve","/opt/thalamus/model"],
            "args":["--served-model-name","example"],
            "ports":[{"name":"http","containerPort":8000,"protocol":"TCP"}]});
        let picker_container = json!({"name":"epp",
            "image":"ghcr.io/llm-d/llm-d-router-endpoint-picker:v1.0.0",
            "command":["/app/epp"],
            "args":["--pool-name",POOL,"--pool-namespace",NS,"--pool-group",
                "inference.networking.k8s.io","--grpc-port=9002"],
            "ports":[{"name":"grpc","containerPort":9002,"protocol":"TCP"},
                {"name":"grpc-health","containerPort":9003,"protocol":"TCP"}],
            "readinessProbe":{"grpc":{"port":9003,"service":"readiness"}}});
        let ready = json!([{"type":"Ready","status":"True","observedGeneration":1,
            "lastTransitionTime":"2024-01-01T00:00:00Z"}]);
        let pod_conditions = json!([{"type":"Initialized","status":"True"},
            {"type":"Ready","status":"True"},{"type":"ContainersReady","status":"True"}]);
        let annotations = json!({"thalamus.ai/deployment":MODEL,"thalamus.ai/revision":REVISION,
            "thalamus.ai/serving-kind":"raw","thalamus.ai/model-manifest":"model-a-manifest"});
        let parent = json!({"group":"gateway.networking.k8s.io","kind":"Gateway",
            "name":"inference-serving","namespace":NS});
        let mut objects = BTreeMap::new();
        objects.insert(
            MODEL_PATH.into(),
            json!({
                "apiVersion":"apps/v1","kind":"Deployment",
                "metadata":{"name":MODEL,"namespace":NS,"uid":MODEL_UID,"generation":1,
                    "creationTimestamp":"2024-01-01T00:00:00Z","labels":flux_labels,
                    "annotations":annotations},
                "spec":{"replicas":1,"selector":{"matchLabels":model_labels},
                    "template":{"metadata":{"labels":model_labels,"annotations":annotations},
                        "spec":{"containers":[model_container]}}},
                "status":{"observedGeneration":1,"replicas":1,"updatedReplicas":1,
                    "readyReplicas":1,"availableReplicas":1}
            }),
        );
        objects.insert(MODEL_RS_PATH.into(), json!({
            "apiVersion":"apps/v1","kind":"ReplicaSet",
            "metadata":{"name":"model-a-6dfb6","namespace":NS,"uid":MODEL_RS_UID,
                "generation":1,"creationTimestamp":"2024-01-01T00:01:00Z",
                "ownerReferences":[owner("apps/v1","Deployment",MODEL,MODEL_UID)]},
            "spec":{"replicas":1,"selector":{"matchLabels":model_pod_labels},
                "template":{"metadata":{"labels":model_pod_labels},
                    "spec":{"containers":[model_container]}}},
            "status":{"observedGeneration":1,"replicas":1,"readyReplicas":1,"availableReplicas":1}
        }));
        objects.insert("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving".into(), json!({
            "apiVersion":"kustomize.toolkit.fluxcd.io/v1","kind":"Kustomization",
            "metadata":{"name":"serving","namespace":"flux-system","uid":"flux-uid","generation":1},
            "spec":{"sourceRef":{"kind":"GitRepository","name":"catalog"}},
            "status":{"lastAppliedRevision":applied,"lastAttemptedRevision":applied,
                "conditions":ready,"inventory":{"entries":[
                    {"id":"inference_model-a_apps_Deployment","v":"v1"},
                    {"id":"inference_model-a-backend_agentgateway.dev_AgentgatewayBackend","v":"v1alpha1"},
                    {"id":"inference_model-a-inference-pool_inference.networking.k8s.io_InferencePool","v":"v1"},
                    {"id":"inference_model-a-benchmark_gateway.networking.k8s.io_HTTPRoute","v":"v1"},
                    {"id":"inference_model-a-epp__Service","v":"v1"},
                    {"id":"inference_model-a-epp_apps_Deployment","v":"v1"}]}}
        }));
        objects.insert("/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/catalog".into(), json!({
            "apiVersion":"source.toolkit.fluxcd.io/v1","kind":"GitRepository",
            "metadata":{"name":"catalog","namespace":"flux-system","uid":"source-uid","generation":1},
            "spec":{"ref":{"branch":"main"}},
            "status":{"artifact":{"revision":applied},"conditions":ready}
        }));
        objects.insert(BACKEND_PATH.into(), json!({
            "apiVersion":"agentgateway.dev/v1alpha1","kind":"AgentgatewayBackend",
            "metadata":{"name":"model-a-backend","namespace":NS,"uid":"backend-uid",
                "generation":1,"labels":flux_labels,"annotations":annotations},
            "spec":{"ai":{"provider":{"custom":{"model":"example",
                "formats":[{"type":"Completions","path":"/v1/chat/completions"}],
                "backendRef":{"group":"inference.networking.k8s.io","kind":"InferencePool","name":POOL}}}},
                "policies":{"transformation":{"response":{"set":[{"name":"x-thalamus-backend",
                    "value":format!("\"example/{MODEL}/{REVISION}\"")}]}}}}
        }));
        objects.insert(POOL_PATH.into(), json!({
            "apiVersion":"inference.networking.k8s.io/v1","kind":"InferencePool",
            "metadata":{"name":POOL,"namespace":NS,"uid":"pool-uid","generation":1,
                "creationTimestamp":"2024-01-01T00:00:00Z","labels":flux_labels},
            "spec":{"appProtocol":"http","targetPorts":[{"number":8000}],
                "selector":{"matchLabels":{"thalamus.ai/deployment":MODEL}},
                "endpointPickerRef":{"kind":"Service","name":PICKER,"port":{"number":9002},"failureMode":"FailClose"}},
            "status":{"parents":[{"parentRef":parent,"controllerName":"example.net/gateway-controller",
                "conditions":[{"type":"Accepted","status":"True","observedGeneration":1,
                    "reason":"Accepted","lastTransitionTime":"2024-01-01T00:02:00Z"},
                    {"type":"ResolvedRefs","status":"True","observedGeneration":1,
                    "reason":"ResolvedRefs","lastTransitionTime":"2024-01-01T00:02:00Z"}]}]}
        }));
        objects.insert(ROUTE_PATH.into(), json!({
            "apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute",
            "metadata":{"name":"model-a-benchmark","namespace":NS,"uid":"route-uid","generation":1,"labels":flux_labels},
            "spec":{"parentRefs":[parent],"hostnames":["inference.example.invalid"],
                "rules":[{"matches":[{"method":"POST","path":{"type":"Exact","value":"/candidates/model-a/v1/completions"}}],
                    "backendRefs":[{"group":"inference.networking.k8s.io","kind":"InferencePool","name":POOL,"weight":1}]}]},
            "status":{"parents":[{"parentRef":parent,"controllerName":"example.net/gateway-controller",
                "conditions":[{"type":"Accepted","status":"True","observedGeneration":1},
                    {"type":"ResolvedRefs","status":"True","observedGeneration":1}]}]}
        }));
        objects.insert(TRAFFIC_ROUTE_PATH.into(), json!({
            "apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute",
            "metadata":{"name":"example","namespace":NS,"uid":"traffic-route-uid","generation":1,"labels":flux_labels},
            "spec":{"parentRefs":[parent],"hostnames":["inference.example.invalid"],
                "rules":[{"backendRefs":[{"group":"agentgateway.dev","kind":"AgentgatewayBackend",
                    "name":"model-a-backend","weight":1}]}]},
            "status":{"parents":[{"parentRef":parent,"controllerName":"example.net/gateway-controller",
                "conditions":[{"type":"Accepted","status":"True","observedGeneration":1},
                    {"type":"ResolvedRefs","status":"True","observedGeneration":1}]}]}
        }));
        objects.insert(
            PICKER_SERVICE_PATH.into(),
            json!({
                "apiVersion":"v1","kind":"Service",
                "metadata":{"name":PICKER,"namespace":NS,"uid":PICKER_UID,
                    "creationTimestamp":"2024-01-01T00:00:00Z","labels":flux_labels},
                "spec":{"selector":picker_labels,"ports":[{"name":"grpc-ext-proc","protocol":"TCP",
                    "appProtocol":"kubernetes.io/h2c","port":9002,"targetPort":9002}]}
            }),
        );
        objects.insert(PICKER_DEPLOYMENT_PATH.into(), json!({
            "apiVersion":"apps/v1","kind":"Deployment",
            "metadata":{"name":PICKER,"namespace":NS,"uid":PICKER_DEPLOYMENT_UID,"generation":1,
                "creationTimestamp":"2024-01-01T00:00:00Z","labels":flux_labels,
                "annotations":{"deployment.kubernetes.io/revision":"1"}},
            "spec":{"replicas":1,"selector":{"matchLabels":picker_labels},
                "template":{"metadata":{"labels":picker_labels},"spec":{"containers":[picker_container]}}},
            "status":{"observedGeneration":1,"replicas":1,"updatedReplicas":1,
                "readyReplicas":1,"availableReplicas":1}
        }));
        objects.insert(PICKER_RS_PATH.into(), json!({
            "apiVersion":"apps/v1","kind":"ReplicaSet",
            "metadata":{"name":"model-a-epp-6dfb6","namespace":NS,"uid":PICKER_RS_UID,"generation":1,
                "creationTimestamp":"2024-01-01T00:01:00Z","annotations":{"deployment.kubernetes.io/revision":"1"},
                "ownerReferences":[owner("apps/v1","Deployment",PICKER,PICKER_DEPLOYMENT_UID)]},
            "spec":{"replicas":1,"selector":{"matchLabels":picker_pod_labels},
                "template":{"metadata":{"labels":picker_pod_labels},"spec":{"containers":[picker_container]}}},
            "status":{"observedGeneration":1,"replicas":1,"readyReplicas":1,"availableReplicas":1}
        }));
        let pods = vec![
            json!({
                "apiVersion":"v1","kind":"Pod",
                "metadata":{"name":"model-a-6dfb6-abcde","namespace":NS,"uid":"model-pod-uid",
                    "creationTimestamp":"2024-01-01T00:02:00Z","labels":model_pod_labels,
                    "annotations":annotations,"ownerReferences":[owner("apps/v1","ReplicaSet","model-a-6dfb6",MODEL_RS_UID)]},
                "spec":{"nodeName":"gpu-node","containers":[model_container]},
                "status":{"phase":"Running","podIP":"10.42.0.11","conditions":pod_conditions,
                    "containerStatuses":[{"name":"main","ready":true,"image":image,
                        "imageID":format!("containerd://sha256:{IMAGE_DIGEST}")}]}
            }),
            json!({
                "apiVersion":"v1","kind":"Pod",
                "metadata":{"name":"model-a-epp-6dfb6-abcde","namespace":NS,"uid":PICKER_POD_UID,
                    "creationTimestamp":"2024-01-01T00:02:00Z","labels":picker_pod_labels,
                    "ownerReferences":[owner("apps/v1","ReplicaSet","model-a-epp-6dfb6",PICKER_RS_UID)]},
                "spec":{"nodeName":"worker-node","containers":[picker_container]},
                "status":{"phase":"Running","podIP":"10.42.0.12","conditions":pod_conditions,
                    "containerStatuses":[{"name":"epp","ready":true,
                        "imageID":format!("containerd://sha256:{PICKER_DIGEST}")}]}
            }),
        ];
        let slices = vec![json!({
            "apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice",
            "metadata":{"name":"model-a-epp-abcde","namespace":NS,"uid":"slice-uid",
                "creationTimestamp":"2024-01-01T00:03:00Z",
                "labels":{"kubernetes.io/service-name":PICKER,
                    "endpointslice.kubernetes.io/managed-by":"endpointslice-controller.k8s.io"},
                "ownerReferences":[owner("v1","Service",PICKER,PICKER_UID)]},
            "addressType":"IPv4","ports":[{"name":"grpc-ext-proc","protocol":"TCP","port":9002}],
            "endpoints":[{"addresses":["10.42.0.12"],"conditions":{"ready":true,"serving":true,"terminating":false},
                "targetRef":{"apiVersion":"v1","kind":"Pod","name":"model-a-epp-6dfb6-abcde",
                    "namespace":NS,"uid":PICKER_POD_UID}}]
        })];
        Self {
            objects,
            pods,
            slices: Some(slices),
        }
    }

    fn kserve() -> Self {
        let mut fixture = Self::ready();
        let mut workload = fixture.objects.remove(MODEL_PATH).unwrap();
        let kserve_uid = "77777777-7777-4777-8777-777777777777";
        let mut annotations = workload["metadata"]["annotations"].clone();
        annotations["thalamus.ai/serving-kind"] = json!("kserve");
        let labels = workload["metadata"]["labels"].clone();
        let containers = workload["spec"]["template"]["spec"]["containers"].clone();
        let workload_labels = json!({"app.kubernetes.io/name":MODEL,
            "app.kubernetes.io/part-of":"llminferenceservice",
            "app.kubernetes.io/component":"llminferenceservice-workload",
            "kserve.io/component":"workload"});
        let mut workload_pod_labels = workload_labels.clone();
        workload_pod_labels["pod-template-hash"] = json!("6dfb6");
        let pool_labels = json!({"app.kubernetes.io/name":MODEL,
            "app.kubernetes.io/part-of":"llminferenceservice","kserve.io/component":"workload"});
        let picker_labels = json!({"app.kubernetes.io/name":MODEL,
            "app.kubernetes.io/part-of":"llminferenceservice",
            "app.kubernetes.io/component":"llminferenceservice-router-scheduler"});
        let mut picker_pod_labels = picker_labels.clone();
        picker_pod_labels["pod-template-hash"] = json!("6dfb6");
        fixture.objects.insert(KSERVE_PATH.into(), json!({
            "apiVersion":"serving.kserve.io/v1alpha2","kind":"LLMInferenceService",
            "metadata":{"name":MODEL,"namespace":NS,"uid":kserve_uid,"generation":1,
                "creationTimestamp":"2024-01-01T00:00:00Z","labels":labels,"annotations":annotations},
            "spec":{"replicas":1,"template":{"containers":containers}},
            "status":{"conditions":[{"type":"Ready","status":"True","observedGeneration":1}]}
        }));
        workload["metadata"]["name"] = json!("model-a-kserve-workload");
        workload["metadata"]["annotations"] = annotations.clone();
        workload["spec"]["template"]["metadata"]["annotations"] = annotations.clone();
        fixture.pods[0]["metadata"]["annotations"] = annotations.clone();
        workload["spec"]["selector"]["matchLabels"] = workload_labels.clone();
        workload["spec"]["template"]["metadata"]["labels"] = workload_labels.clone();
        fixture.pods[0]["metadata"]["labels"] = workload_pod_labels.clone();
        fixture.objects.get_mut(MODEL_RS_PATH).unwrap()["spec"]["selector"]["matchLabels"] =
            workload_pod_labels.clone();
        fixture.objects.get_mut(MODEL_RS_PATH).unwrap()["spec"]["template"]["metadata"]["labels"] =
            workload_pod_labels;
        workload["metadata"]["ownerReferences"] = json!([owner(
            "serving.kserve.io/v1alpha2",
            "LLMInferenceService",
            MODEL,
            kserve_uid
        )]);
        fixture
            .objects
            .insert(KSERVE_DEPLOYMENT_PATH.into(), workload);
        fixture.objects.get_mut(MODEL_RS_PATH).unwrap()["metadata"]["ownerReferences"] =
            json!([owner(
                "apps/v1",
                "Deployment",
                "model-a-kserve-workload",
                MODEL_UID
            )]);
        fixture.objects.get_mut(BACKEND_PATH).unwrap()["metadata"]["annotations"] = annotations;
        fixture.objects.get_mut(POOL_PATH).unwrap()["metadata"]["ownerReferences"] =
            json!([owner(
                "serving.kserve.io/v1alpha2",
                "LLMInferenceService",
                MODEL,
                kserve_uid
            )]);
        fixture.objects.get_mut(POOL_PATH).unwrap()["spec"]["selector"]["matchLabels"] =
            pool_labels;
        fixture.objects.get_mut(POOL_PATH).unwrap()["spec"]["endpointPickerRef"]["name"] =
            json!(KSERVE_PICKER_SERVICE_NAME);
        fixture.objects.get_mut(POOL_PATH).unwrap()["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("labels");
        for path in [PICKER_SERVICE_PATH, PICKER_DEPLOYMENT_PATH] {
            let metadata = fixture.objects.get_mut(path).unwrap()["metadata"]
                .as_object_mut()
                .unwrap();
            metadata.remove("labels");
            metadata.insert(
                "ownerReferences".into(),
                json!([owner(
                    "serving.kserve.io/v1alpha2",
                    "LLMInferenceService",
                    MODEL,
                    kserve_uid
                )]),
            );
        }
        let mut picker_service = fixture.objects.remove(PICKER_SERVICE_PATH).unwrap();
        picker_service["metadata"]["name"] = json!(KSERVE_PICKER_SERVICE_NAME);
        picker_service["spec"]["selector"] = picker_labels.clone();
        picker_service["spec"]["ports"][0]["name"] = json!("grpc");
        picker_service["spec"]["ports"][0]["targetPort"] = json!("grpc");
        fixture
            .objects
            .insert(KSERVE_PICKER_SERVICE_PATH.into(), picker_service);

        // KServe's scheduler preset exposes a named grpc port on container
        // "main"; this repository overrides its command for llm-d router v1.
        let mut picker_container = fixture.pods[1]["spec"]["containers"][0].clone();
        picker_container["name"] = json!("main");
        picker_container["command"] = json!([
            "/app/epp",
            "--pool-name",
            POOL,
            "--pool-namespace",
            NS,
            "--zap-encoder",
            "json",
            "--grpc-port=9002",
            "--grpc-health-port",
            "9003",
            "--secure-serving=false",
            "--metrics-endpoint-auth=false"
        ]);
        picker_container.as_object_mut().unwrap().remove("args");
        let mut picker_deployment = fixture.objects.remove(PICKER_DEPLOYMENT_PATH).unwrap();
        picker_deployment["metadata"]["name"] = json!(KSERVE_PICKER_DEPLOYMENT_NAME);
        picker_deployment["spec"]["selector"]["matchLabels"] = picker_labels.clone();
        picker_deployment["spec"]["template"]["metadata"]["labels"] = picker_labels;
        picker_deployment["spec"]["template"]["spec"]["containers"][0] = picker_container.clone();
        fixture
            .objects
            .insert(KSERVE_PICKER_DEPLOYMENT_PATH.into(), picker_deployment);
        let mut picker_rs = fixture.objects.remove(PICKER_RS_PATH).unwrap();
        picker_rs["metadata"]["name"] = json!(KSERVE_PICKER_RS_NAME);
        picker_rs["metadata"]["ownerReferences"][0]["name"] = json!(KSERVE_PICKER_DEPLOYMENT_NAME);
        picker_rs["spec"]["selector"]["matchLabels"] = picker_pod_labels.clone();
        picker_rs["spec"]["template"]["metadata"]["labels"] = picker_pod_labels.clone();
        picker_rs["spec"]["template"]["spec"]["containers"][0] = picker_container.clone();
        fixture
            .objects
            .insert(KSERVE_PICKER_RS_PATH.into(), picker_rs);
        fixture.pods[1]["metadata"]["name"] = json!(KSERVE_PICKER_POD_NAME);
        fixture.pods[1]["metadata"]["labels"] = picker_pod_labels;
        fixture.pods[1]["metadata"]["ownerReferences"][0]["name"] = json!(KSERVE_PICKER_RS_NAME);
        fixture.pods[1]["spec"]["containers"][0] = picker_container;
        fixture.pods[1]["status"]["containerStatuses"][0]["name"] = json!("main");
        let slice = &mut fixture.slices.as_mut().unwrap()[0];
        slice["metadata"]["labels"]["kubernetes.io/service-name"] =
            json!(KSERVE_PICKER_SERVICE_NAME);
        slice["metadata"]["ownerReferences"][0]["name"] = json!(KSERVE_PICKER_SERVICE_NAME);
        slice["ports"][0]["name"] = json!("grpc");
        slice["endpoints"][0]["targetRef"]["name"] = json!(KSERVE_PICKER_POD_NAME);
        fixture.objects.insert(KSERVE_SERVICE_PATH.into(), json!({
            "apiVersion":"v1","kind":"Service",
            "metadata":{"name":"model-a-kserve-workload-svc","namespace":NS,"uid":"workload-service-uid",
                "ownerReferences":[owner("serving.kserve.io/v1alpha2","LLMInferenceService",MODEL,kserve_uid)]},
            "spec":{"selector":workload_labels,
                "ports":[{"name":"http","port":8000,"targetPort":8000,"protocol":"TCP"}]}
        }));
        fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")
            .unwrap()["status"]["inventory"]["entries"][0]["id"] =
            json!("inference_model-a_serving.kserve.io_LLMInferenceService");
        fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")
            .unwrap()["status"]["inventory"]["entries"][0]["v"] = json!("v1alpha2");
        fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")
            .unwrap()["status"]["inventory"]["entries"].as_array_mut().unwrap().retain(|entry| {
                !matches!(entry["id"].as_str(),
                    Some("inference_model-a-inference-pool_inference.networking.k8s.io_InferencePool")
                    | Some("inference_model-a-epp__Service")
                    | Some("inference_model-a-epp_apps_Deployment"))
            });
        fixture
    }

    fn add_filler(&mut self) {
        let labels = json!({"thalamus.ai/deployment":MODEL,"thalamus.ai/pool-role":"filler"});
        let pod_labels = json!({"thalamus.ai/deployment":MODEL,"thalamus.ai/pool-role":"filler",
            "pod-template-hash":"6dfb6"});
        let mut deployment = self.objects.get(MODEL_PATH).unwrap().clone();
        deployment["metadata"]["name"] = json!("model-a-filler");
        deployment["metadata"]["uid"] = json!(FILLER_UID);
        deployment["spec"]["selector"]["matchLabels"] = labels.clone();
        deployment["spec"]["template"]["metadata"]["labels"] = labels;
        self.objects.insert(FILLER_PATH.into(), deployment);
        let mut rs = self.objects.get(MODEL_RS_PATH).unwrap().clone();
        rs["metadata"]["name"] = json!("model-a-filler-6dfb6");
        rs["metadata"]["uid"] = json!(FILLER_RS_UID);
        rs["metadata"]["ownerReferences"] =
            json!([owner("apps/v1", "Deployment", "model-a-filler", FILLER_UID)]);
        rs["spec"]["selector"]["matchLabels"] = pod_labels.clone();
        rs["spec"]["template"]["metadata"]["labels"] = pod_labels.clone();
        self.objects.insert(FILLER_RS_PATH.into(), rs);
        let mut pod = self.pods[0].clone();
        pod["metadata"]["name"] = json!("model-a-filler-6dfb6-abcde");
        pod["metadata"]["uid"] = json!("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        pod["metadata"]["labels"] = pod_labels;
        pod["metadata"]["ownerReferences"] = json!([owner(
            "apps/v1",
            "ReplicaSet",
            "model-a-filler-6dfb6",
            FILLER_RS_UID
        )]);
        pod["status"]["podIP"] = json!("10.42.0.14");
        self.pods.push(pod);
        self.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")
            .unwrap()["status"]["inventory"]["entries"].as_array_mut().unwrap()
            .push(json!({"id":"inference_model-a-filler_apps_Deployment","v":"v1"}));
    }

    fn mark_picker_unready(&mut self) {
        self.pods[1]["status"]["conditions"][1]["status"] = json!("False");
        self.pods[1]["status"]["containerStatuses"][0]["ready"] = json!(false);
    }

    fn scenario(name: &str) -> Option<Self> {
        let mut fixture = Self::ready();
        match name {
            "ready" => {}
            "filler-ready" => fixture.add_filler(),
            "filler-wrong-revision" => {
                fixture.add_filler();
                fixture.objects.get_mut(FILLER_PATH)?["metadata"]["annotations"]
                    ["thalamus.ai/revision"] = json!(PICKER_DIGEST);
            }
            "filler-wrong-image" => {
                fixture.add_filler();
                fixture.pods[2]["status"]["containerStatuses"][0]["imageID"] =
                    json!(format!("containerd://sha256:{PICKER_DIGEST}"));
            }
            "pool-unrelated-pod" => {
                fixture.add_filler();
                let mut unrelated = fixture.objects.get(FILLER_PATH)?.clone();
                unrelated["metadata"]["name"] = json!("other-workload");
                unrelated["metadata"]["uid"] = json!("cccccccc-cccc-4ccc-8ccc-cccccccccccc");
                fixture.objects.insert(
                    "/apis/apps/v1/namespaces/inference/deployments/other-workload".into(),
                    unrelated,
                );
                fixture.objects.get_mut(FILLER_RS_PATH)?["metadata"]["ownerReferences"] =
                    json!([owner(
                        "apps/v1",
                        "Deployment",
                        "other-workload",
                        "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
                    )]);
            }
            "referenced-names" => {
                let mut pool = fixture.objects.remove(POOL_PATH)?;
                pool["metadata"]["name"] = json!("live-pool");
                pool["spec"]["endpointPickerRef"]["name"] = json!("live-picker");
                fixture.objects.insert("/apis/inference.networking.k8s.io/v1/namespaces/inference/inferencepools/live-pool".into(), pool);
                fixture.objects.get_mut(BACKEND_PATH)?["spec"]["ai"]["provider"]["custom"]
                    ["backendRef"]["name"] = json!("live-pool");
                fixture.objects.get_mut(ROUTE_PATH)?["spec"]["rules"][0]["backendRefs"][0]
                    ["name"] = json!("live-pool");
                fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")?
                    ["status"]["inventory"]["entries"][2]["id"] = json!("inference_live-pool_inference.networking.k8s.io_InferencePool");
                let mut picker_service = fixture.objects.remove(PICKER_SERVICE_PATH)?;
                picker_service["metadata"]["name"] = json!("live-picker");
                fixture.objects.insert(
                    "/api/v1/namespaces/inference/services/live-picker".into(),
                    picker_service,
                );
                fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")?
                    ["status"]["inventory"]["entries"][4]["id"] = json!("inference_live-picker__Service");
                fixture.slices.as_mut()?[0]["metadata"]["labels"]["kubernetes.io/service-name"] =
                    json!("live-picker");
                fixture.slices.as_mut()?[0]["metadata"]["ownerReferences"][0]["name"] =
                    json!("live-picker");
                fixture.pods[1]["spec"]["containers"][0]["args"][1] = json!("live-pool");
                fixture.objects.get_mut(PICKER_RS_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"][1] = json!("live-pool");
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"][1] = json!("live-pool");
            }
            "missing-pool" => {
                fixture.objects.remove(POOL_PATH);
            }
            "missing-backend" => {
                fixture.objects.remove(BACKEND_PATH);
            }
            "wrong-backend-revision" => {
                fixture.objects.get_mut(BACKEND_PATH)?["metadata"]["annotations"]
                    ["thalamus.ai/revision"] = json!(PICKER_DIGEST)
            }
            "missing-picker-service" => {
                fixture.objects.remove(PICKER_SERVICE_PATH);
            }
            "picker-service-wrong-flux" => {
                fixture.objects.get_mut(PICKER_SERVICE_PATH)?["metadata"]["labels"]
                    ["kustomize.toolkit.fluxcd.io/name"] = json!("other")
            }
            "picker-deployment-wrong-flux" => {
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["metadata"]["labels"]
                    ["kustomize.toolkit.fluxcd.io/name"] = json!("other")
            }
            "picker-service-missing-inventory" => {
                fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")?
                    ["status"]["inventory"]["entries"].as_array_mut()?.remove(4);
            }
            "picker-deployment-missing-inventory" => {
                fixture.objects.get_mut("/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/serving")?
                    ["status"]["inventory"]["entries"].as_array_mut()?.remove(5);
            }
            "pool-false" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["status"] = json!("False")
            }
            "resolved-false" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][1]
                    ["status"] = json!("False")
            }
            "missing-accepted-resolved-false" => {
                let conditions = fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]
                    ["conditions"]
                    .as_array_mut()?;
                conditions.remove(0);
                conditions[0]["status"] = json!("False");
            }
            "accepted-false-missing-resolved" => {
                let conditions = fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]
                    ["conditions"]
                    .as_array_mut()?;
                conditions[0]["status"] = json!("False");
                conditions.remove(1);
            }
            "stale-accepted-resolved-false" => {
                let conditions = fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]
                    ["conditions"]
                    .as_array_mut()?;
                conditions[0]["observedGeneration"] = json!(0);
                conditions[1]["status"] = json!("False");
            }
            "missing-accepted-stale-resolved" => {
                let conditions = fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]
                    ["conditions"]
                    .as_array_mut()?;
                conditions.remove(0);
                conditions[0]["observedGeneration"] = json!(0);
            }
            "stale-pool" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["observedGeneration"] = json!(0)
            }
            "pool-unknown" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["status"] = json!("Unknown")
            }
            "missing-pool-parent" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]
                    .as_object_mut()?
                    .remove("parents");
            }
            "wrong-pool-parent" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["parentRef"]["name"] =
                    json!("other-gateway")
            }
            "duplicate-pool-parent" => {
                let parents =
                    fixture.objects.get_mut(POOL_PATH)?["status"]["parents"].as_array_mut()?;
                parents.push(parents[0].clone());
            }
            "missing-condition" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"]
                    .as_array_mut()?
                    .remove(1);
            }
            "duplicate-condition" => {
                let conditions = fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]
                    ["conditions"]
                    .as_array_mut()?;
                conditions.push(conditions[0].clone());
            }
            "wrong-target" => {
                fixture.objects.get_mut(ROUTE_PATH)?["spec"]["rules"][0]["backendRefs"][0]["name"] =
                    json!("other-pool")
            }
            "wrong-selector" => {
                fixture.objects.get_mut(POOL_PATH)?["spec"]["selector"]["matchLabels"]
                    ["thalamus.ai/deployment"] = json!("another-model")
            }
            "wrong-target-port" => {
                fixture.objects.get_mut(POOL_PATH)?["spec"]["targetPorts"][0]["number"] =
                    json!(8001)
            }
            "missing-controller" => {
                fixture.objects.get_mut(TRAFFIC_ROUTE_PATH)?["status"]["parents"][0]
                    .as_object_mut()?
                    .remove("controllerName");
            }
            "wrong-controller" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["controllerName"] =
                    json!("other.example/controller")
            }
            "traffic-false" => {
                fixture.objects.get_mut(TRAFFIC_ROUTE_PATH)?["status"]["parents"][0]["conditions"]
                    [0]["status"] = json!("False")
            }
            "stale-traffic" => {
                fixture.objects.get_mut(TRAFFIC_ROUTE_PATH)?["status"]["parents"][0]["conditions"]
                    [0]["observedGeneration"] = json!(0)
            }
            "stale-traffic-pool-false" => {
                fixture.objects.get_mut(TRAFFIC_ROUTE_PATH)?["status"]["parents"][0]
                    ["conditions"][0]["observedGeneration"] = json!(0);
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["status"] = json!("False");
            }
            "wrong-controller-pool-false" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["controllerName"] =
                    json!("other.example/controller");
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["status"] = json!("False");
            }
            "stale-pool-picker-unready" => {
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["observedGeneration"] = json!(0);
                fixture.mark_picker_unready();
            }
            "pool-selector-unknown-picker-unready" => {
                fixture.objects.get_mut(POOL_PATH)?["spec"]
                    .as_object_mut()?
                    .remove("selector");
                fixture.mark_picker_unready();
            }
            "picker-unready-missing-endpoints" => {
                fixture.mark_picker_unready();
                fixture.slices = None;
            }
            "wrong-picker-owner-pod-unready" => {
                fixture.pods[1]["metadata"]["ownerReferences"][0]["uid"] =
                    json!("old-picker-rs-uid");
                fixture.mark_picker_unready();
            }
            "unknown-model" => {
                fixture.pods[0]["status"]["conditions"]
                    .as_array_mut()?
                    .retain(|condition| condition["type"] != "Ready");
            }
            "unknown-model-pool-false" => {
                fixture.pods[0]["status"]["conditions"]
                    .as_array_mut()?
                    .retain(|condition| condition["type"] != "Ready");
                fixture.objects.get_mut(POOL_PATH)?["status"]["parents"][0]["conditions"][0]
                    ["status"] = json!("False");
            }
            "unready-model-missing-pool" => {
                fixture.pods[0]["status"]["conditions"][1]["status"] = json!("False");
                fixture.objects.remove(POOL_PATH);
            }
            "missing-endpoints" => fixture.slices = None,
            "empty-endpoints" => fixture.slices = Some(vec![]),
            "owned-empty-endpoints" => fixture.slices.as_mut()?[0]["endpoints"] = json!([]),
            "picker-unready" => {
                fixture.slices.as_mut()?[0]["endpoints"][0]["conditions"]["ready"] = json!(false);
                fixture.slices.as_mut()?[0]["endpoints"][0]["conditions"]["serving"] = json!(false);
            }
            "picker-pod-unready" => fixture.mark_picker_unready(),
            "picker-grpc-port-wrong"
            | "picker-grpc-port-missing"
            | "picker-grpc-port-split"
            | "picker-grpc-port-equals"
            | "picker-grpc-port-duplicate"
            | "picker-grpc-port-malformed" => {
                let mut args = json!([
                    "--pool-name",
                    POOL,
                    "--pool-namespace",
                    NS,
                    "--pool-group",
                    "inference.networking.k8s.io"
                ]);
                let words = args.as_array_mut()?;
                match name {
                    "picker-grpc-port-wrong" => words.extend([json!("--grpc-port"), json!("9004")]),
                    "picker-grpc-port-split" => words.extend([json!("--grpc-port"), json!("9002")]),
                    "picker-grpc-port-equals" => words.push(json!("--grpc-port=9002")),
                    "picker-grpc-port-duplicate" => words.extend([
                        json!("--grpc-port"),
                        json!("9002"),
                        json!("--grpc-port=9002"),
                    ]),
                    "picker-grpc-port-malformed" => {
                        words.extend([json!("--grpc-port"), json!("not-a-port")])
                    }
                    _ => {}
                }
                fixture.pods[1]["spec"]["containers"][0]["args"] = args.clone();
                fixture.objects.get_mut(PICKER_RS_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"] = args.clone();
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"] = args;
            }
            "picker-image-changed" => {
                fixture.pods[1]["spec"]["containers"][0]["image"] =
                    json!("registry.example/old-picker:1")
            }
            "mixed-picker-digests" => {
                let mut pod = fixture.pods[1].clone();
                pod["metadata"]["name"] = json!("model-a-epp-6dfb6-fghij");
                pod["metadata"]["uid"] = json!("88888888-8888-4888-8888-888888888888");
                pod["status"]["podIP"] = json!("10.42.0.13");
                pod["status"]["containerStatuses"][0]["imageID"] =
                    json!(format!("containerd://sha256:{IMAGE_DIGEST}"));
                fixture.pods.push(pod);
                let workload = fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?;
                workload["spec"]["replicas"] = json!(2);
                for field in [
                    "replicas",
                    "updatedReplicas",
                    "readyReplicas",
                    "availableReplicas",
                ] {
                    workload["status"][field] = json!(2);
                }
                let rs = fixture.objects.get_mut(PICKER_RS_PATH)?;
                rs["spec"]["replicas"] = json!(2);
                rs["status"]["replicas"] = json!(2);
                rs["status"]["readyReplicas"] = json!(2);
                rs["status"]["availableReplicas"] = json!(2);
                fixture.slices.as_mut()?[0]["endpoints"].as_array_mut()?.push(json!({
                    "addresses":["10.42.0.13"],"conditions":{"ready":true,"serving":true,"terminating":false},
                    "targetRef":{"apiVersion":"v1","kind":"Pod","name":"model-a-epp-6dfb6-fghij",
                        "namespace":NS,"uid":"88888888-8888-4888-8888-888888888888"}
                }));
            }
            "picker-missing-owner" => fixture.pods[1]["metadata"]["ownerReferences"] = json!([]),
            "picker-duplicate-flags" => {
                let args = json!([
                    "--pool-name",
                    POOL,
                    "--pool-name",
                    POOL,
                    "--pool-namespace",
                    NS,
                    "--grpc-port",
                    "9002"
                ]);
                fixture.pods[1]["spec"]["containers"][0]["args"] = args.clone();
                fixture.objects.get_mut(PICKER_RS_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"] = args.clone();
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"] = args;
            }
            "stale-picker" => {
                fixture.pods[1]["spec"]["containers"][0]["args"][1] = json!("other-pool");
                fixture.objects.get_mut(PICKER_RS_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"][1] = json!("other-pool");
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["spec"]["template"]["spec"]
                    ["containers"][0]["args"][1] = json!("other-pool");
            }
            "stale-picker-rollout" => {
                fixture.objects.get_mut(PICKER_DEPLOYMENT_PATH)?["metadata"]["generation"] =
                    json!(2)
            }
            "wrong-picker-uid" => {
                fixture.slices.as_mut()?[0]["endpoints"][0]["targetRef"]["uid"] =
                    json!("old-picker-pod-uid")
            }
            "stale-model-replicaset" => {
                fixture.objects.get_mut(MODEL_RS_PATH)?["spec"]["template"]["spec"]["containers"]
                    [0]["image"] = json!("registry.example/old-model:1")
            }
            "stale-picker-replicaset" => {
                fixture.objects.get_mut(PICKER_RS_PATH)?["spec"]["template"]["spec"]["containers"]
                    [0]["image"] = json!("registry.example/old-picker:1")
            }
            "wrong-model-owner" => {
                fixture.pods[0]["metadata"]["ownerReferences"][0]["uid"] = json!("old-model-rs-uid")
            }
            "wrong-model-config" => {
                fixture.pods[0]["spec"]["containers"][0]["args"][1] = json!("another-model")
            }
            "model-extra-command" | "model-extra-args" => {
                let field = if name == "model-extra-command" {
                    "command"
                } else {
                    "args"
                };
                for path in [MODEL_PATH, MODEL_RS_PATH] {
                    fixture.objects.get_mut(path)?["spec"]["template"]["spec"]["containers"][0]
                        .as_object_mut()?
                        .remove(field);
                }
            }
            "model-missing-command" | "model-missing-args" => {
                let field = if name == "model-missing-command" {
                    "command"
                } else {
                    "args"
                };
                fixture.pods[0]["spec"]["containers"][0]
                    .as_object_mut()?
                    .remove(field);
            }
            "wrong-model-image" => {
                fixture.pods[0]["status"]["containerStatuses"][0]["imageID"] =
                    json!(format!("containerd://sha256:{PICKER_DIGEST}"))
            }
            "old-flux" => fixture.objects.get_mut(
                "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/catalog",
            )?["status"]["artifact"]["revision"] =
                json!(format!("main@sha1:{OTHER_COMMIT}")),
            _ => return None,
        }
        Some(fixture)
    }
}

fn api_error(code: StatusCode) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({"apiVersion":"v1","kind":"Status","status":"Failure",
        "message":code.canonical_reason().unwrap_or("API error"),
        "reason":if code == StatusCode::NOT_FOUND { "NotFound" } else { "ServiceUnavailable" },
        "code":code.as_u16()})),
    )
}

async fn kube_api(State(fixture): State<Arc<Fixture>>, uri: Uri) -> (StatusCode, Json<Value>) {
    let path = uri.path();
    if path == PODS_PATH {
        return (
            StatusCode::OK,
            Json(json!({"apiVersion":"v1","kind":"PodList",
            "metadata":{"resourceVersion":"1"},"items":&fixture.pods})),
        );
    }
    if path == SLICES_PATH {
        return match &fixture.slices {
            Some(slices) => (
                StatusCode::OK,
                Json(json!({"apiVersion":"discovery.k8s.io/v1",
                "kind":"EndpointSliceList","metadata":{"resourceVersion":"1"},"items":slices})),
            ),
            None => api_error(StatusCode::SERVICE_UNAVAILABLE),
        };
    }
    match fixture.objects.get(path) {
        Some(object) => (StatusCode::OK, Json(object.clone())),
        None => api_error(StatusCode::NOT_FOUND),
    }
}

struct FixtureServer {
    observer: LiveObserver,
    config: RuntimeConfig,
    task: JoinHandle<()>,
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_fixture(fixture: Fixture) -> FixtureServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .fallback(kube_api)
        .with_state(Arc::new(fixture));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config = runtime_config();
    let kube_config = kube::Config::new(format!("http://{address}").parse().unwrap());
    let client = thalamus_k8s::client_from_config(kube_config).unwrap();
    let observer = LiveObserver::new(client, config.clone()).unwrap();
    FixtureServer {
        observer,
        config,
        task,
    }
}

async fn observed(observer: &LiveObserver, config: &RuntimeConfig) -> (StatusView, StatusView) {
    let service = catalog();
    deployment_observation(observer, config, &service, &service.deployments[0], COMMIT).await
}

async fn assert_case(name: &str, state: StatusValue) {
    let server = start_fixture(Fixture::scenario(name).unwrap()).await;
    let (applied, running) = observed(&server.observer, &server.config).await;
    assert_eq!(applied.state, StatusValue::Yes, "{name}: applied");
    assert_eq!(
        running.state, state,
        "{name}: running: {:?}",
        running.reason
    );
}

#[tokio::test]
async fn running_requires_a_live_pool_and_selected_picker() {
    let server = start_fixture(Fixture::ready()).await;
    let (applied, running) = observed(&server.observer, &server.config).await;
    assert_eq!(
        (applied.state, running.state),
        (StatusValue::Yes, StatusValue::Yes)
    );
}

#[tokio::test]
async fn pool_and_picker_names_are_discovered_from_live_references() {
    let server = start_fixture(Fixture::scenario("referenced-names").unwrap()).await;
    let (applied, running) = observed(&server.observer, &server.config).await;
    assert_eq!(
        (applied.state, running.state),
        (StatusValue::Yes, StatusValue::Yes)
    );
}

#[tokio::test]
async fn candidate_and_kserve_use_their_actual_routes_and_controller_chain() {
    let mut fixture = Fixture::kserve();
    let mut candidate = fixture.objects.get(TRAFFIC_ROUTE_PATH).unwrap().clone();
    candidate["metadata"]["name"] = json!("model-a-candidate");
    fixture
        .objects
        .insert(CANDIDATE_ROUTE_PATH.into(), candidate);
    fixture.objects.remove(TRAFFIC_ROUTE_PATH);
    let server = start_fixture(fixture).await;
    let mut service = catalog();
    service.pool = None;
    service.deployments[0].pooled = false;
    service.deployments[0].role = Some("candidate".into());
    let (applied, running) = deployment_observation(
        &server.observer,
        &server.config,
        &service,
        &service.deployments[0],
        COMMIT,
    )
    .await;
    assert_eq!(
        (applied.state, running.state),
        (StatusValue::Yes, StatusValue::Yes)
    );
}

#[tokio::test]
async fn kserve_owner_and_workload_generation_cannot_be_superseded() {
    for scenario in [
        "pool-owner",
        "workload-owner",
        "workload-generation",
        "picker-service-owner",
        "picker-deployment-owner",
        "unrelated-pod",
    ] {
        let mut fixture = Fixture::kserve();
        match scenario {
            "pool-owner" => {
                fixture.objects.get_mut(POOL_PATH).unwrap()["metadata"]["ownerReferences"][0]
                    ["uid"] = json!("old-kserve-uid");
            }
            "workload-owner" => {
                fixture.objects.get_mut(KSERVE_DEPLOYMENT_PATH).unwrap()["metadata"]
                    ["ownerReferences"][0]["uid"] = json!("old-kserve-uid");
            }
            "picker-service-owner" => {
                fixture.objects.get_mut(KSERVE_PICKER_SERVICE_PATH).unwrap()["metadata"]
                    ["ownerReferences"][0]["uid"] = json!("old-kserve-uid");
            }
            "picker-deployment-owner" => {
                fixture
                    .objects
                    .get_mut(KSERVE_PICKER_DEPLOYMENT_PATH)
                    .unwrap()["metadata"]["ownerReferences"][0]["uid"] = json!("old-kserve-uid");
            }
            "unrelated-pod" => {
                let mut pod = fixture.pods[0].clone();
                pod["metadata"]["name"] = json!("foreign-pod");
                pod["metadata"]["uid"] = json!("dddddddd-dddd-4ddd-8ddd-dddddddddddd");
                pod["metadata"]["labels"]["app.kubernetes.io/component"] = json!("other");
                fixture.pods.push(pod);
            }
            _ => {
                fixture.objects.get_mut(KSERVE_DEPLOYMENT_PATH).unwrap()["metadata"]
                    ["generation"] = json!(2);
            }
        };
        let server = start_fixture(fixture).await;
        let mut service = catalog();
        service.pool = None;
        service.deployments[0].pooled = false;
        let (applied, running) = deployment_observation(
            &server.observer,
            &server.config,
            &service,
            &service.deployments[0],
            COMMIT,
        )
        .await;
        assert_eq!(
            (applied.state, running.state),
            (StatusValue::Yes, StatusValue::Stale),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn pool_parent_requires_current_conditions_and_the_expected_controller() {
    assert_case("missing-pool", StatusValue::Unknown).await;
    assert_case("missing-backend", StatusValue::Unknown).await;
    assert_case("wrong-backend-revision", StatusValue::Stale).await;
    for name in ["pool-false", "resolved-false"] {
        assert_case(name, StatusValue::No).await;
    }
    for name in [
        "missing-accepted-resolved-false",
        "accepted-false-missing-resolved",
        "stale-accepted-resolved-false",
    ] {
        assert_case(name, StatusValue::No).await;
    }
    assert_case("missing-accepted-stale-resolved", StatusValue::Stale).await;
    assert_case("stale-pool", StatusValue::Stale).await;
    for name in [
        "missing-pool-parent",
        "wrong-pool-parent",
        "duplicate-pool-parent",
    ] {
        assert_case(name, StatusValue::Unknown).await;
    }
    for name in ["pool-unknown", "missing-condition", "duplicate-condition"] {
        assert_case(name, StatusValue::Unknown).await;
    }
    assert_case("missing-controller", StatusValue::Unknown).await;
    assert_case("wrong-controller", StatusValue::Stale).await;
    assert_case("traffic-false", StatusValue::No).await;
    assert_case("stale-traffic", StatusValue::Stale).await;
}

#[tokio::test]
async fn pool_must_target_this_workload_and_benchmark_route() {
    assert_case("wrong-target", StatusValue::Stale).await;
    assert_case("wrong-selector", StatusValue::Stale).await;
    assert_case("wrong-target-port", StatusValue::Stale).await;
}

#[tokio::test]
async fn pool_allows_only_current_attributed_filler_pods() {
    let server = start_fixture(Fixture::scenario("filler-ready").unwrap()).await;
    let (applied, running) = observed(&server.observer, &server.config).await;
    assert_eq!(
        (applied.state, running.state),
        (StatusValue::Yes, StatusValue::Yes)
    );
    drop(server);
    assert_case("pool-unrelated-pod", StatusValue::Stale).await;
    assert_case("filler-wrong-revision", StatusValue::Stale).await;
    assert_case("filler-wrong-image", StatusValue::Stale).await;
}

#[tokio::test]
async fn picker_evidence_distinguishes_unavailable_unready_and_superseded() {
    assert_case("missing-picker-service", StatusValue::Unknown).await;
    assert_case("picker-service-wrong-flux", StatusValue::Stale).await;
    assert_case("picker-deployment-wrong-flux", StatusValue::Stale).await;
    assert_case("picker-service-missing-inventory", StatusValue::Unknown).await;
    assert_case("picker-deployment-missing-inventory", StatusValue::Unknown).await;
    assert_case("empty-endpoints", StatusValue::Unknown).await;
    assert_case("missing-endpoints", StatusValue::Unknown).await;
    assert_case("picker-unready", StatusValue::No).await;
    assert_case("owned-empty-endpoints", StatusValue::No).await;
    assert_case("picker-pod-unready", StatusValue::No).await;
    assert_case("picker-missing-owner", StatusValue::Unknown).await;
    assert_case("picker-duplicate-flags", StatusValue::Unknown).await;
    assert_case("picker-image-changed", StatusValue::Stale).await;
    assert_case("mixed-picker-digests", StatusValue::Stale).await;
    assert_case("stale-picker-rollout", StatusValue::Stale).await;
    assert_case("stale-picker-replicaset", StatusValue::Stale).await;
    assert_case("stale-picker", StatusValue::Stale).await;
    assert_case("wrong-picker-uid", StatusValue::Stale).await;
}

#[tokio::test]
async fn picker_listener_port_is_observed_from_the_real_container() {
    assert_case("picker-grpc-port-wrong", StatusValue::Stale).await;
    assert_case("picker-grpc-port-missing", StatusValue::Unknown).await;
    assert_case("picker-grpc-port-split", StatusValue::Yes).await;
    assert_case("picker-grpc-port-equals", StatusValue::Yes).await;
    assert_case("picker-grpc-port-duplicate", StatusValue::Unknown).await;
    assert_case("picker-grpc-port-malformed", StatusValue::Unknown).await;
}

#[tokio::test]
async fn pool_health_never_replaces_revision_image_or_pod_ownership_evidence() {
    assert_case("wrong-model-owner", StatusValue::Stale).await;
    assert_case("stale-model-replicaset", StatusValue::Stale).await;
    assert_case("wrong-model-config", StatusValue::Stale).await;
    for name in ["model-extra-command", "model-extra-args"] {
        assert_case(name, StatusValue::Stale).await;
    }
    for name in ["model-missing-command", "model-missing-args"] {
        assert_case(name, StatusValue::Unknown).await;
    }
    assert_case("unknown-model", StatusValue::Unknown).await;
    assert_case("wrong-model-image", StatusValue::Stale).await;

    let server = start_fixture(Fixture::scenario("old-flux").unwrap()).await;
    let (applied, running) = observed(&server.observer, &server.config).await;
    assert_eq!(
        (applied.state, running.state),
        (StatusValue::Stale, StatusValue::Stale)
    );
}

#[tokio::test]
async fn independent_failures_take_precedence_over_unknown_evidence() {
    assert_case("unknown-model-pool-false", StatusValue::No).await;
    assert_case("unready-model-missing-pool", StatusValue::No).await;
    assert_case("stale-traffic-pool-false", StatusValue::No).await;
    assert_case("picker-unready-missing-endpoints", StatusValue::No).await;
    assert_case("stale-pool-picker-unready", StatusValue::No).await;
    assert_case("pool-selector-unknown-picker-unready", StatusValue::No).await;
    assert_case("wrong-controller-pool-false", StatusValue::Stale).await;
    assert_case("wrong-picker-owner-pod-unready", StatusValue::Stale).await;
}
