use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use thalamus_core::*;

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_string()).collect()
}

fn profile() -> HardwareProfile {
    HardwareProfile {
        name: "example-gpu".into(),
        gpu_product: "example".into(),
        gpu_memory_bytes: 80,
        gpus_per_node: 8,
        interconnect: "example".into(),
        cpu_architecture: "x86_64".into(),
        host_memory_bytes: 128,
        shared_memory: false,
        driver_stack: "example".into(),
        node_labels: BTreeMap::new(),
    }
}

fn service() -> PublishedService {
    PublishedService {
        name: "example".into(),
        protocols: BTreeSet::from([EngineProtocol::OpenAiChat]),
        deployments: vec![
            ServiceDeployment {
                deployment: "a".into(),
                weight: 100,
            },
            ServiceDeployment {
                deployment: "b".into(),
                weight: 0,
            },
        ],
        access_policy: "example-policy".into(),
        owners: vec!["example-team".into()],
    }
}

fn validation(name: &str, context: u32, features: &[&str]) -> ValidatedCapabilities {
    ValidatedCapabilities {
        deployment: name.into(),
        revision: "revision-1".into(),
        run_id: format!("run-{name}"),
        contract: CapabilityContract {
            context_tokens: context,
            features: set(features),
            protocols: BTreeSet::from([EngineProtocol::OpenAiChat]),
        },
    }
}

fn eligible(name: &str) -> EligibleDeployment {
    EligibleDeployment {
        deployment: name.into(),
        revision: "revision-1".into(),
    }
}

#[test]
fn published_contract_includes_zero_weight_target_and_intersects_proven_features() {
    let service = service();
    let deployments = vec![eligible("a"), eligible("b")];
    let validations = vec![
        validation("a", 8192, &["tools", "reasoning"]),
        validation("b", 4096, &["tools"]),
    ];
    let published = published_capabilities(&service, &deployments, &validations).unwrap();
    assert_eq!(published.contract.context_tokens, 4096);
    assert_eq!(published.contract.features, set(&["tools"]));
    assert!(published_capabilities(&service, &deployments[..1], &validations).is_err());
    let mut stale = validations.clone();
    stale[1].revision = "old".into();
    assert!(published_capabilities(&service, &deployments, &stale).is_err());
    let mut missing_protocol = validations.clone();
    missing_protocol[1].contract.protocols.clear();
    assert_eq!(
        published_capabilities(&service, &deployments, &missing_protocol),
        Err(CapabilityError::UnsupportedProtocol)
    );
}

#[test]
fn published_protocols_are_limited_to_service_exposure() {
    let deployments = vec![eligible("a"), eligible("b")];
    let mut validations = vec![
        validation("a", 8192, &["tools", "reasoning"]),
        validation("b", 4096, &["tools"]),
    ];
    for validation in &mut validations {
        validation
            .contract
            .protocols
            .insert(EngineProtocol::AnthropicMessages);
    }

    let published = published_capabilities(&service(), &deployments, &validations).unwrap();
    assert_eq!(
        published.contract.protocols,
        BTreeSet::from([EngineProtocol::OpenAiChat])
    );
    assert_eq!(published.contract.context_tokens, 4096);
    assert_eq!(published.contract.features, set(&["tools"]));
}

#[test]
fn fragmented_gpu_headroom_and_fillers_choose_correct_mode() {
    let fragmented = vec![
        NodeGpuCapacity {
            node: "a".into(),
            free_gpus: 2,
            preemptible_filler_gpus: 0,
        },
        NodeGpuCapacity {
            node: "b".into(),
            free_gpus: 2,
            preemptible_filler_gpus: 0,
        },
    ];
    assert_eq!(
        select_rollout_mode(&profile(), &fragmented, 4, 1, true)
            .unwrap()
            .mode,
        RolloutMode::InPlace
    );
    let nodes = vec![NodeGpuCapacity {
        node: "a".into(),
        free_gpus: 2,
        preemptible_filler_gpus: 6,
    }];
    assert_eq!(
        select_rollout_mode(&profile(), &nodes, 4, 2, true)
            .unwrap()
            .mode,
        RolloutMode::Gated
    );
    assert_eq!(
        select_rollout_mode(&profile(), &nodes, 4, 3, true)
            .unwrap()
            .mode,
        RolloutMode::InPlace
    );
    assert_eq!(
        select_rollout_mode(&profile(), &nodes, 4, 3, false)
            .unwrap()
            .mode,
        RolloutMode::Rolling
    );
    let invalid = vec![NodeGpuCapacity {
        node: "a".into(),
        free_gpus: 8,
        preemptible_filler_gpus: 1,
    }];
    assert!(matches!(
        select_rollout_mode(&profile(), &invalid, 4, 1, true),
        Err(CapacityError::ExceedsNodeCapacity(_))
    ));
}

fn evidence() -> EvidenceIdentity {
    EvidenceIdentity {
        desired: DesiredIdentity {
            rendered_resources: "render-digest".into(),
            resolved_dependencies: "dependencies-digest".into(),
        },
        runtime: RuntimeIdentity {
            image_digests: BTreeMap::from([(
                "engine".into(),
                format!("sha256:{}", "a".repeat(64)),
            )]),
            model_content: "model-digest".into(),
            tokenizer_content: "tokenizer-digest".into(),
            effective_configuration: "config-digest".into(),
            component_versions: BTreeMap::from([("gateway".into(), "1.0".into())]),
        },
        test: TestIdentity {
            suite: "suite-1".into(),
            runner: "runner-1".into(),
            client: "client-1".into(),
            workload: "workload-1".into(),
            seed: 42,
        },
        conditions: RunConditions {
            hardware: "gpu-driver-1".into(),
            replicas: 1,
            cache_state: "cold".into(),
            competing_traffic: "none".into(),
            time: "night-window".into(),
        },
    }
}

#[test]
fn evidence_requires_complete_matching_four_part_identity_and_success() {
    let identity = evidence();
    assert!(identity.permits_reuse_of(&identity));
    let mut different = identity.clone();
    different.runtime.model_content = "different-model".into();
    assert!(!identity.permits_reuse_of(&different));
    different = identity.clone();
    different.test.seed += 1;
    assert!(!identity.permits_reuse_of(&different));
    different = identity.clone();
    different.conditions.cache_state = "warm".into();
    assert!(!identity.permits_reuse_of(&different));
    different = identity.clone();
    different.desired.resolved_dependencies.clear();
    assert!(!different.permits_reuse_of(&different));
    different = identity.clone();
    different
        .runtime
        .image_digests
        .insert("engine".into(), "latest".into());
    assert!(!different.permits_reuse_of(&different));
    let run = Run {
        id: "run".into(),
        idempotency_key: "key".into(),
        deployment: "a".into(),
        revision: "r1".into(),
        kind: RunKind::Benchmark,
        outcome: RunOutcome::Failed,
        started_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .to_utc(),
        identity: Some(identity.clone()),
        results_uri: None,
    };
    assert!(!run.is_reusable_for(RunKind::Benchmark, &identity));
    let run = Run {
        outcome: RunOutcome::Passed,
        ..run
    };
    assert!(run.is_reusable_for(RunKind::Benchmark, &identity));
    assert!(!run.is_reusable_for(RunKind::Validation, &identity));
}

#[test]
fn independent_statuses_round_trip_including_stale_and_unknown() {
    let observation = |value| StatusObservation {
        value,
        revision: "r1".into(),
        observed_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .to_utc(),
        reason: "example observation".into(),
    };
    let statuses = DeploymentStatuses {
        accepted: observation(StatusValue::Yes),
        applied: observation(StatusValue::Unknown),
        running: observation(StatusValue::Stale),
        functional: observation(StatusValue::No),
        measured: observation(StatusValue::Unknown),
    };
    let json = serde_json::to_string(&statuses).unwrap();
    assert_eq!(
        serde_json::from_str::<DeploymentStatuses>(&json).unwrap(),
        statuses
    );
}

proptest! {
    #[test]
    fn intersection_never_exceeds_either_validated_context(a in 1u32..100_000, b in 1u32..100_000) {
        let result = published_capabilities(&service(), &[eligible("a"), eligible("b")], &[validation("a", a, &["tools"]), validation("b", b, &["tools", "reasoning"])]).unwrap();
        prop_assert_eq!(result.contract.context_tokens, a.min(b));
        prop_assert_eq!(result.contract.features, set(&["tools"]));
    }
    #[test]
    fn fragmented_capacity_counts_only_whole_units(a in 0u32..=8, b in 0u32..=8, size in 1u32..=8) {
        let nodes = vec![NodeGpuCapacity { node: "a".into(), free_gpus: a, preemptible_filler_gpus: 0 }, NodeGpuCapacity { node: "b".into(), free_gpus: b, preemptible_filler_gpus: 0 }];
        let result = select_rollout_mode(&profile(), &nodes, size, 1, true).unwrap();
        prop_assert_eq!(result.available_units, u64::from(a / size + b / size));
        prop_assert_eq!(result.mode == RolloutMode::Gated, a / size + b / size >= 1);
    }
}
