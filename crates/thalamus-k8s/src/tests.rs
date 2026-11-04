use super::*;
use serde_json::json;

fn object(name: &str, conditions: serde_json::Value) -> DynamicObject {
    serde_json::from_value(json!({"apiVersion": "helm.toolkit.fluxcd.io/v2", "kind": "HelmRelease", "metadata": {"name": name, "namespace": "test"}, "status": {"conditions": conditions}})).unwrap()
}

#[test]
fn conditions_are_three_valued_and_tolerant_of_partial_status() {
    let obj = object(
        "app",
        json!([
            {"type": "Ready", "status": "True", "observedGeneration": 4, "lastTransitionTime": "2025-01-01T01:00:00+01:00"},
            {"type": "Reconciling", "status": "False", "reason": "Done"},
            {"type": "Future", "status": "Maybe", "lastTransitionTime": "bad"},
            {"type": "Missing"}, {"status": "True"}, 42
        ]),
    );
    let decoded = conditions(&obj);
    assert_eq!(decoded.len(), 4);
    assert_eq!(decoded[0].status, ConditionStatus::True);
    assert_eq!(decoded[0].observed_generation, Some(4));
    assert_eq!(
        decoded[0].last_transition_time.unwrap().to_rfc3339(),
        "2025-01-01T00:00:00+00:00"
    );
    assert_eq!(decoded[1].status, ConditionStatus::False);
    assert_eq!(decoded[2].status, ConditionStatus::Unknown);
    assert!(decoded[2].last_transition_time.is_none());
    assert_eq!(decoded[3].status, ConditionStatus::Unknown);
    assert!(conditions(&object("app", json!({}))).is_empty());
}

#[test]
fn watch_relist_is_atomic_and_disconnect_invalidates_freshness() {
    let now = Instant::now();
    let mut state = WatchState::default();
    state.apply(Event::Init, now);
    state.apply(Event::InitApply(object("old", json!([]))), now);
    assert!(!state.is_fresh(now, Duration::from_secs(10)));
    state.apply(Event::InitDone, now);
    assert!(state.is_fresh(now, Duration::from_secs(10)));
    assert!(!state.is_fresh(now + Duration::from_secs(11), Duration::from_secs(10)));
    state.apply(Event::Init, now);
    state.apply(Event::InitApply(object("new", json!([]))), now);
    assert!(state.objects().contains_key("old"));
    assert!(!state.is_fresh(now, Duration::from_secs(10)));
    state.apply(Event::InitDone, now);
    assert!(!state.objects().contains_key("old"));
    assert!(state.objects().contains_key("new"));
    state.invalidate();
    state.apply(Event::Apply(object("new", json!([]))), now);
    assert!(!state.is_fresh(now, Duration::from_secs(10)));
}

#[test]
fn projection_checks_names_keys_size_and_ownership() {
    assert!(projection("user-map", BTreeMap::new()).is_err());
    assert!(projection("thalamus-", BTreeMap::new()).is_err());
    assert!(
        projection(
            "thalamus-keys",
            BTreeMap::from([("bad/key".into(), "x".into())])
        )
        .is_err()
    );
    let map = projection(
        "thalamus-keys",
        BTreeMap::from([("keys.json".into(), "x".repeat(CONFIG_MAP_MAX_BYTES))]),
    )
    .unwrap();
    assert_eq!(
        map.metadata.labels.unwrap().get(MANAGED_BY).unwrap(),
        MANAGER
    );
    assert!(
        projection(
            "thalamus-keys",
            BTreeMap::from([("keys".into(), "x".repeat(CONFIG_MAP_MAX_BYTES + 1))])
        )
        .is_err()
    );
    assert!(matches!(
        ensure_owned(&ObjectMeta::default()),
        Err(Error::NotOwned)
    ));
    let mut metadata = ObjectMeta {
        labels: Some(BTreeMap::from([(MANAGED_BY.into(), "flux".into())])),
        ..Default::default()
    };
    assert!(matches!(set_owned(&mut metadata), Err(Error::NotOwned)));
}

#[test]
fn lease_takeover_requires_a_proven_expiry_and_preserves_transitions() {
    let now = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut lease = Lease {
        spec: Some(LeaseSpec {
            holder_identity: Some("one".into()),
            lease_duration_seconds: Some(30),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(2),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(matches!(
        update_lease(&mut lease, "two", 30, now + chrono::Duration::seconds(29)),
        Err(Error::LeaseHeld)
    ));
    update_lease(&mut lease, "two", 30, now + chrono::Duration::seconds(30)).unwrap();
    assert_eq!(lease.spec.as_ref().unwrap().lease_transitions, Some(3));
    update_lease(&mut lease, "two", 30, now + chrono::Duration::seconds(31)).unwrap();
    assert_eq!(lease.spec.as_ref().unwrap().lease_transitions, Some(3));
    lease.spec.as_mut().unwrap().renew_time = None;
    lease.spec.as_mut().unwrap().acquire_time = None;
    assert!(matches!(
        update_lease(&mut lease, "three", 30, now + chrono::Duration::days(1)),
        Err(Error::LeaseHeld)
    ));
}

#[test]
fn read_resources_use_explicit_upstream_identity() {
    let resource = ReadKind::HelmRelease.resource("v2").unwrap();
    assert_eq!(resource.group, "helm.toolkit.fluxcd.io");
    assert_eq!(resource.plural, "helmreleases");
    assert!(ReadKind::InferencePool.resource("../v1").is_err());
}

type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, String, serde_json::Value)>>>;

fn fake_client(responses: Vec<(u16, serde_json::Value)>) -> (K8s, RecordedRequests) {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        sync::{Arc, Mutex},
    };
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let responses = responses.clone();
        let recorded = recorded.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect_bytes().await.unwrap();
            let body = if bytes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            recorded
                .lock()
                .unwrap()
                .push((parts.method.to_string(), parts.uri.to_string(), body));
            let (status, response) = responses.lock().unwrap().pop_front().unwrap();
            Ok::<_, Infallible>(
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(
                        serde_json::to_vec(&response).unwrap(),
                    ))
                    .unwrap(),
            )
        }
    });
    (
        K8s::new(Client::new(service, "default"), "test").unwrap(),
        requests,
    )
}

fn managed_projection(data: serde_json::Value, fields: serde_json::Value) -> serde_json::Value {
    json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {
            "name": "thalamus-projection", "namespace": "test", "resourceVersion": "7",
            "labels": { "app.kubernetes.io/managed-by": "thalamus" },
            "managedFields": fields
        },
        "data": data
    })
}

#[tokio::test]
async fn projection_create_is_empty_and_revocation_uses_non_forced_apply() {
    let fields = json!([{"manager":"thalamus","operation":"Apply","apiVersion":"v1",
        "fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:old":{},"f:keep":{}}}}]);
    let initial = managed_projection(json!({"old":"x","keep":"y"}), fields);
    let empty = managed_projection(json!({}), json!([]));
    let revoked = managed_projection(json!({"keep":"y"}), json!([]));
    let (k8s, requests) = fake_client(vec![
        (
            404,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        ),
        (201, empty),
        (200, initial.clone()),
        (200, initial),
        (200, revoked),
    ]);
    k8s.apply_projection(
        "thalamus-projection",
        BTreeMap::from([("old".into(), "x".into()), ("keep".into(), "y".into())]),
    )
    .await
    .unwrap();
    k8s.apply_projection(
        "thalamus-projection",
        BTreeMap::from([("keep".into(), "y".into())]),
    )
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[1].0, "POST");
    assert!(
        requests[1]
            .1
            .starts_with("/api/v1/namespaces/test/configmaps?")
    );
    assert!(requests[1].2.get("data").is_none());
    assert_eq!(requests[1].2["metadata"]["labels"][MANAGED_BY], MANAGER);
    for request in [&requests[2], &requests[4]] {
        assert_eq!(request.0, "PATCH");
        assert!(request.1.contains("fieldManager=thalamus"));
        assert!(!request.1.contains("force=true"));
        assert_eq!(request.2["metadata"]["resourceVersion"], "7");
    }
    assert_eq!(requests[2].2["data"]["old"], "x");
    assert!(requests[4].2["data"].get("old").is_none());
    assert_eq!(requests[4].2["data"]["keep"], "y");
}

#[tokio::test]
async fn projection_selector_labels_are_validated_and_applied_after_creation() {
    for (key, value) in [
        (MANAGED_BY, "thalamus"),
        ("Bad_domain/name", "x"),
        ("example.com/", "x"),
        ("example.com/name", "-bad"),
        ("example.com/name", "bad/value"),
    ] {
        let (k8s, requests) = fake_client(vec![]);
        assert!(matches!(
            k8s.apply_projection_with_labels(
                "thalamus-projection",
                BTreeMap::new(),
                BTreeMap::from([(key.into(), value.into())])
            )
            .await,
            Err(Error::Invalid(_))
        ));
        assert!(requests.lock().unwrap().is_empty());
    }
    let object = managed_projection(json!({}), json!([]));
    let (k8s, requests) = fake_client(vec![
        (
            404,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        ),
        (201, object.clone()),
        (200, object),
    ]);
    k8s.apply_projection_with_labels(
        "thalamus-projection",
        BTreeMap::new(),
        BTreeMap::from([("gateway.example.com/config".into(), "primary".into())]),
    )
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert!(
        requests[1].2["metadata"]["labels"]
            .get("gateway.example.com/config")
            .is_none()
    );
    assert_eq!(
        requests[2].2["metadata"]["labels"]["gateway.example.com/config"],
        "primary"
    );
    assert_eq!(requests[2].2["metadata"]["labels"][MANAGED_BY], MANAGER);
    assert_eq!(requests[2].2["metadata"]["resourceVersion"], "7");
    assert!(!requests[2].1.contains("force=true"));
}

#[tokio::test]
async fn projection_selector_labels_reject_unknown_and_shared_ownership() {
    for fields in [
        json!([]),
        json!([
            {"manager":"thalamus","operation":"Apply","fieldsType":"FieldsV1",
             "fieldsV1":{"f:metadata":{"f:labels":{"f:gateway.example.com/config":{}}}}},
            {"manager":"other","operation":"Apply","fieldsType":"FieldsV1",
             "fieldsV1":{"f:metadata":{"f:labels":{"f:gateway.example.com/config":{}}}}}
        ]),
    ] {
        let mut object = managed_projection(json!({}), fields);
        object["metadata"]["labels"]["gateway.example.com/config"] = json!("primary");
        let (k8s, requests) = fake_client(vec![(200, object)]);
        assert!(matches!(
            k8s.apply_projection_with_labels(
                "thalamus-projection",
                BTreeMap::new(),
                BTreeMap::from([("gateway.example.com/config".into(), "primary".into())])
            )
            .await,
            Err(Error::UnsafeProjectionOwnership)
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    let fields = json!([{"manager":"thalamus","operation":"Apply","fieldsType":"FieldsV1",
        "fieldsV1":{"f:metadata":{"f:labels":{"f:gateway.example.com/config":{}}}}}]);
    let mut object = managed_projection(json!({}), fields);
    object["metadata"]["labels"]["gateway.example.com/config"] = json!("primary");
    let (k8s, requests) = fake_client(vec![(200, object.clone()), (200, object)]);
    k8s.apply_projection_with_labels(
        "thalamus-projection",
        BTreeMap::new(),
        BTreeMap::from([("gateway.example.com/config".into(), "other".into())]),
    )
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests[1].2["metadata"]["labels"]["gateway.example.com/config"],
        "other"
    );
    assert_eq!(requests[1].2["metadata"]["resourceVersion"], "7");
}

#[tokio::test]
async fn legacy_dual_owned_data_fails_before_apply_even_for_revocation() {
    let fields = json!([
        {"manager":"thalamus","operation":"Update","apiVersion":"v1","fieldsType":"FieldsV1",
         "fieldsV1":{"f:data":{"f:old":{}}}},
        {"manager":"thalamus","operation":"Apply","apiVersion":"v1","fieldsType":"FieldsV1",
         "fieldsV1":{"f:data":{"f:old":{}}}}
    ]);
    let (k8s, requests) = fake_client(vec![(200, managed_projection(json!({"old":"x"}), fields))]);
    assert!(matches!(
        k8s.apply_projection("thalamus-projection", BTreeMap::new())
            .await,
        Err(Error::UnsafeProjectionOwnership)
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_or_shared_data_ownership_fails_closed() {
    for fields in [
        json!([]),
        json!([{"manager":"other","operation":"Apply","fieldsType":"FieldsV1",
            "fieldsV1":{"f:data":{"f:old":{}}}},
            {"manager":"thalamus","operation":"Apply","fieldsType":"FieldsV1",
            "fieldsV1":{"f:data":{"f:old":{}}}}]),
    ] {
        let (k8s, requests) =
            fake_client(vec![(200, managed_projection(json!({"old":"x"}), fields))]);
        assert!(matches!(
            k8s.apply_projection("thalamus-projection", BTreeMap::new())
                .await,
            Err(Error::UnsafeProjectionOwnership)
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn projection_rejects_unowned_or_unversioned_objects_and_propagates_conflicts() {
    let mut unowned = managed_projection(json!({}), json!([]));
    unowned["metadata"]["labels"][MANAGED_BY] = json!("someone-else");
    let mut unversioned = managed_projection(json!({}), json!([]));
    unversioned["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("resourceVersion");
    for (object, is_unowned) in [(unowned, true), (unversioned, false)] {
        let (k8s, requests) = fake_client(vec![(200, object)]);
        let result = k8s
            .apply_projection("thalamus-projection", BTreeMap::new())
            .await;
        if is_unowned {
            assert!(matches!(result, Err(Error::NotOwned)));
        } else {
            assert!(matches!(result, Err(Error::Invalid(_))));
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    let (k8s, requests) = fake_client(vec![
        (200, managed_projection(json!({}), json!([]))),
        (
            409,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Conflict","code":409}),
        ),
    ]);
    assert!(matches!(
        k8s.apply_projection("thalamus-projection", BTreeMap::from([("new".into(), "x".into())])).await,
        Err(Error::Kube(kube::Error::Api(error))) if error.code == 409
    ));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].1.contains("force=true"));
    assert_eq!(requests[1].2["metadata"]["resourceVersion"], "7");
}
