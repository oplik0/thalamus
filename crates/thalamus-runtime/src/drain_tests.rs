use super::*;
use chrono::Utc;
use serde_json::json;

#[test]
fn filler_state_requires_zero_owned_sets_pods_and_scalers() {
    let (config, mut resources, _) = super::tests::fixture(false);
    resources.pools[0]["spec"]["selector"]["matchLabels"] = json!({"thalamus.ai/deployment":"dep"});
    resources.serving[0]["spec"]["template"]["metadata"] = json!({"labels":{
        "thalamus.ai/deployment":"dep","thalamus.ai/pool-role":"normal","kueue.x-k8s.io/queue-name":"pool-member"}});
    let mut filler = resources.serving[0].clone();
    filler["metadata"]["name"] = json!("dep-filler");
    filler["metadata"]["uid"] = json!("filler-uid");
    filler["spec"]["replicas"] = json!(0);
    filler["spec"]["selector"] =
        json!({"matchLabels":{"thalamus.ai/deployment":"dep","thalamus.ai/pool-role":"filler"}});
    filler["spec"]["template"]["metadata"]["labels"] = json!({"thalamus.ai/deployment":"dep","thalamus.ai/pool-role":"filler","kueue.x-k8s.io/queue-name":"pool-member-filler"});
    filler["status"] = json!({"observedGeneration":1,"replicas":0});
    resources.serving.push(filler);
    resources.kustomizations[0]["status"]["inventory"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"work_dep-filler_apps_Deployment"}));
    let make_rs = || {
        json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"filler-rs","namespace":"work","uid":"filler-rs-uid","generation":1,
        "ownerReferences":[{"kind":"Deployment","name":"dep-filler","uid":"filler-uid","controller":true}]},
        "spec":{"replicas":0},"status":{"replicas":0,"observedGeneration":1}})
    };
    resources.replicasets.push(make_rs());
    let observe =
        |resources: &LiveResources| join_filler_state(&config, "svc", "dep", resources, Utc::now());
    let zero = observe(&resources).unwrap();
    assert!(zero.verified_zero());
    assert_eq!(zero.serving_uid, "deployment-uid");
    assert_eq!(zero.filler_uid, "filler-uid");
    assert_eq!(zero.pool, "dep-inference-pool");
    assert_eq!(zero.queue, "pool-member-filler");
    resources.replicasets[0]["spec"]["replicas"] = json!(1);
    assert!(!observe(&resources).unwrap().verified_zero());
    resources.replicasets[0] = make_rs();
    resources.pods.push(json!({"kind":"Pod","metadata":{"namespace":"work","name":"terminating","uid":"pod-x","deletionTimestamp":"2026-01-01T00:00:00Z",
        "ownerReferences":[{"kind":"ReplicaSet","uid":"filler-rs-uid","controller":true}]},"status":{"phase":"Pending"}}));
    let pending = observe(&resources).unwrap();
    assert!(!pending.verified_zero());
    assert!(pending.pods[0].terminating);
    resources.pods.pop();
    resources.horizontal_autoscalers.push(json!({"kind":"HorizontalPodAutoscaler","metadata":{"name":"other","namespace":"work","uid":"hpa"},
        "spec":{"scaleTargetRef":{"kind":"Deployment","name":"dep-filler"}}}));
    assert!(!observe(&resources).unwrap().verified_zero());
    resources.horizontal_autoscalers.clear();
    resources.scaled_objects.push(json!({"kind":"ScaledObject","metadata":{"name":"filler-scale","namespace":"work","uid":"scale"},
        "spec":{"scaleTargetRef":{"name":"dep-filler"}}}));
    assert!(!observe(&resources).unwrap().verified_zero());
    resources.scaled_objects.clear();
    resources.serving[1]["status"]["observedGeneration"] = json!(0);
    assert!(observe(&resources).is_err());
}
