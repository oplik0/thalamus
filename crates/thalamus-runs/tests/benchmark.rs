use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use thalamus_runs::{
    compare_benchmark_results, job_name, parse_benchmark_result, BenchmarkPercentiles,
    BenchmarkStage, EvidenceIdentity, Run, RunConditions, RunKind, RunRequest, Workload,
    BENCHMARK_MAX_POINTS, BENCHMARK_MAX_RESULT_BYTES, INFERENCE_PERF_IMAGE,
};
use uuid::Uuid;

const ADAPTER_IMAGE: &str =
    "example.org/adapter@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn run() -> Run {
    let id = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
    let at = Utc.timestamp_opt(100, 0).unwrap();
    let request = RunRequest {
        project_id: Uuid::from_u128(100),
        kind: RunKind::Benchmark,
        service: "inference".into(),
        deployment: "baseline".into(),
        runner: "perf".into(),
        workload: Workload {
            name: "random".into(),
            seed: 42,
            concurrency: 8,
            duration_seconds: 60,
            warmup_seconds: 1,
            max_requests: 100,
            model_budget_tokens: 10_000,
            tool_budget_calls: 0,
        },
        evidence: EvidenceIdentity {
            desired_configuration: "sha256:desired-a".into(),
            observed_runtime: "sha256:runtime-a".into(),
            test_definition: "sha256:test".into(),
            conditions: RunConditions {
                hardware: "gpu-a".into(),
                nodes: vec!["node-a".into()],
                replicas: 1,
                cache_state: "warm".into(),
                competing_traffic: "low".into(),
                observed_at: at,
            },
        },
    };
    let mut run = Run {
        id,
        team: "team-a".into(),
        actor: "user".into(),
        idempotency_key: "one".into(),
        fingerprint: request.fingerprint().unwrap(),
        request,
        job_name: job_name(id),
        job_uid: None,
        ownership_nonce: Uuid::from_u128(2),
        job_spec: None,
        job_namespace: None,
        state: "succeeded".into(),
        artifacts: None,
        error: None,
        created_at: at,
        updated_at: at,
    };
    let result = evidence(&run);
    bind_job(&mut run, &result);
    run
}

fn evidence(run: &Run) -> Value {
    json!({
        "schema_version": 1,
        "run_id": run.id.to_string(),
        "request": run.request,
        "runner": {
            "name": "inference-perf",
            "version": "0.7.0",
            "adapter_version": "1",
            "image": ADAPTER_IMAGE,
            "upstream_image": INFERENCE_PERF_IMAGE
        },
        "tokenizer": {
            "repository": "org/tokenizer",
            "revision": "0123456789abcdef0123456789abcdef01234567"
        },
        "definition": {
            "name": run.request.workload.name,
            "seed": run.request.workload.seed,
            "warmup_seconds": run.request.workload.warmup_seconds,
            "spec": {
                "load": {"type": "constant", "stages": [
                    {"rate": 2, "duration": 3},
                    {"rate": 4, "duration": 2}
                ]},
                "data": {"type": "random", "input_len": 128, "output_len": 64},
                "goodput": {"ttft": 0.5, "itl": 0.1}
            }
        },
        "points": [
            {
                "stage": {"type": "constant", "rate": 2, "duration": 3},
                "successes": 5,
                "failures": 1,
                "achieved_request_rate": 1.9,
                "ttft_seconds": {"p50": 0.2, "p95": 0.4},
                "itl_seconds": {"p50": 0.02, "p95": 0.04},
                "throughput": {"requests_per_second": 1.5, "output_tokens_per_second": 31.0},
                "goodput": {"requests_per_second": 1.25}
            },
            {
                "stage": {"type": "constant", "rate": 4, "duration": 2},
                "successes": 0,
                "failures": 8,
                "achieved_request_rate": null,
                "ttft_seconds": null,
                "itl_seconds": null,
                "throughput": {"requests_per_second": 0.0, "output_tokens_per_second": null},
                "goodput": null
            }
        ]
    })
}

fn bind_job(run: &mut Run, value: &Value) {
    let image = value["runner"]["image"].as_str().unwrap();
    let mut workloads = serde_json::Map::new();
    workloads.insert(
        run.request.workload.name.clone(),
        value["definition"]["spec"].clone(),
    );
    let config = json!({
        "base_url": "https://inference.example/v1",
        "model": "fixture",
        "benchmark": {"tokenizer": value["tokenizer"], "workloads": workloads}
    });
    run.job_spec = Some(json!({
        "template": {"spec": {"containers": [{
            "name": "runner",
            "image": image,
            "env": [
                {"name": "THALAMUS_RUNNER_IMAGE", "value": image},
                {"name": "THALAMUS_BENCHMARK_CONFIG", "value": config.to_string()}
            ]
        }]}}
    }));
}

fn parse(run: &Run, value: &Value) -> thalamus_runs::Result<thalamus_runs::BenchmarkResult> {
    parse_benchmark_result(run, &serde_json::to_vec(value).unwrap())
}

fn candidate() -> Run {
    let mut run = run();
    run.id = Uuid::parse_str("bbbbbbbb-cccc-4ddd-8eee-ffffffffffff").unwrap();
    run.job_name = job_name(run.id);
    run
}

#[test]
fn parses_exact_curves_and_keeps_unavailable_measurements_null() {
    let run = run();
    let value = evidence(&run);
    let parsed = parse(&run, &value).unwrap();
    assert_eq!(parsed.points.len(), 2);
    assert_eq!(parsed.points[0].successes, 5);
    assert_eq!(parsed.points[0].failures, 1);
    assert_eq!(parsed.points[0].achieved_request_rate, Some(1.9));
    assert_eq!(parsed.points[0].ttft_seconds.as_ref().unwrap().p95, 0.4);
    assert_eq!(
        parsed.points[0].throughput.output_tokens_per_second,
        Some(31.0)
    );
    assert_eq!(parsed.points[1].achieved_request_rate, None);
    assert_eq!(parsed.points[1].ttft_seconds, None);
    assert_eq!(parsed.points[1].itl_seconds, None);
    assert_eq!(parsed.points[1].throughput.output_tokens_per_second, None);
    assert_eq!(parsed.points[1].goodput, None);
    let encoded = serde_json::to_value(&parsed).unwrap();
    assert_eq!(encoded["definition"]["spec"], value["definition"]["spec"]);
    assert_eq!(encoded["points"], value["points"]);
}

#[test]
fn rejects_wrong_version_forged_identity_noncanonical_ids_and_unknown_fields() {
    let run = run();
    for version in [0, 2, u32::MAX] {
        let mut value = evidence(&run);
        value["schema_version"] = json!(version);
        assert!(parse(&run, &value).is_err());
    }
    let mut value = evidence(&run);
    value["run_id"] = json!(candidate().id.to_string());
    assert!(parse(&run, &value).is_err());
    value["run_id"] = json!(run.id.to_string().to_uppercase());
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["request"]["workload"]["seed"] = json!(43);
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["request"]["service"] = json!("another-service");
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["request"]["evidence"]["conditions"]["cache_state"] = json!("cold");
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["request"]["extra"] = json!("not part of the run");
    assert!(parse(&run, &value).is_err());
    let encoded = serde_json::to_string(&evidence(&run)).unwrap();
    let project = format!("\"project_id\":\"{}\"", run.request.project_id);
    let duplicate = format!("\"project_id\":\"{}\",{project}", Uuid::from_u128(999));
    let encoded = encoded.replacen(&project, &duplicate, 1);
    assert!(parse_benchmark_result(&run, encoded.as_bytes()).is_err());
    value = evidence(&run);
    value["score"] = json!(42);
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value.as_object_mut().unwrap().remove("points");
    value["score"] = json!(42);
    assert!(parse(&run, &value).is_err());
    assert!(parse_benchmark_result(&run, &vec![b' '; BENCHMARK_MAX_RESULT_BYTES + 1]).is_err());
}

#[test]
fn validates_runner_tokenizer_and_requested_definition() {
    let run = run();
    for (field, bad) in [
        ("name", "another-runner"),
        ("version", "0.8.0"),
        ("adapter_version", "2"),
        ("upstream_image", "quay.io/not-pinned:latest"),
    ] {
        let mut value = evidence(&run);
        value["runner"][field] = json!(bad);
        assert!(parse(&run, &value).is_err());
    }
    for repository in [
        "",
        "https://user:secret@example.com/model",
        "org/a/b",
        "org/../model",
        "bad--name",
    ] {
        let mut value = evidence(&run);
        value["tokenizer"]["repository"] = json!(repository);
        assert!(parse(&run, &value).is_err());
    }
    for revision in [
        "main",
        "ABCDEF0123456789abcdef0123456789abcdef01",
        "0123456789abcdef0123456789abcdef0123456g",
    ] {
        let mut value = evidence(&run);
        value["tokenizer"]["revision"] = json!(revision);
        assert!(parse(&run, &value).is_err());
    }
    for (field, bad) in [
        ("name", json!("other")),
        ("seed", json!(44)),
        ("warmup_seconds", json!(0)),
    ] {
        let mut value = evidence(&run);
        value["definition"][field] = bad;
        assert!(parse(&run, &value).is_err());
    }
    let mut non_benchmark = run.clone();
    non_benchmark.request.kind = RunKind::Validation;
    assert!(parse(&non_benchmark, &evidence(&non_benchmark)).is_err());
}

#[test]
fn result_is_bound_to_the_persisted_runner_image_and_named_config() {
    let original = run();
    let value = evidence(&original);
    assert!(parse(&original, &value).is_ok());

    let mut missing = original.clone();
    missing.job_spec = None;
    assert!(parse(&missing, &value).is_err());

    let mut different_image = original.clone();
    different_image.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0]["image"] =
        json!("example.org/adapter@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert!(parse(&different_image, &value).is_err());

    let mut wrong_env = original.clone();
    wrong_env.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0]["env"][0]["value"] =
        json!("example.org/adapter:mutable");
    assert!(parse(&wrong_env, &value).is_err());

    let mut ambiguous = original.clone();
    let env = ambiguous.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0]["env"]
        .as_array_mut()
        .unwrap();
    env.push(json!({"name":"THALAMUS_RUNNER_IMAGE", "value":ADAPTER_IMAGE}));
    assert!(parse(&ambiguous, &value).is_err());

    let mut no_config = original.clone();
    no_config.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0]["env"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert!(parse(&no_config, &value).is_err());

    let mut changed_config = original.clone();
    let container =
        &mut changed_config.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0];
    let mut config: Value =
        serde_json::from_str(container["env"][1]["value"].as_str().unwrap()).unwrap();
    config["benchmark"]["tokenizer"]["revision"] =
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    container["env"][1]["value"] = json!(config.to_string());
    assert!(parse(&changed_config, &value).is_err());

    let mut changed_workload = original.clone();
    let container =
        &mut changed_workload.job_spec.as_mut().unwrap()["template"]["spec"]["containers"][0];
    let mut config: Value =
        serde_json::from_str(container["env"][1]["value"].as_str().unwrap()).unwrap();
    config["benchmark"]["workloads"]["random"]["goodput"]["ttft"] = json!(0.6);
    container["env"][1]["value"] = json!(config.to_string());
    assert!(parse(&changed_workload, &value).is_err());

    let mut unsafe_result = value.clone();
    unsafe_result["runner"]["image"] = json!("example.org/user:secret@registry/adapter@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let mut unsafe_job = original.clone();
    bind_job(&mut unsafe_job, &unsafe_result);
    assert!(parse(&unsafe_job, &unsafe_result).is_err());
}

#[test]
fn rejects_invalid_measurements_in_json_and_in_memory() {
    let run = run();
    for (field, bad) in [
        ("ttft_seconds", json!({"p50": -0.1, "p95": 0.4})),
        ("ttft_seconds", json!({"p50": 0.5, "p95": 0.4})),
        ("itl_seconds", json!({"p50": 0.1, "p95": -0.1})),
        ("achieved_request_rate", json!(-1.0)),
        ("goodput", json!({"requests_per_second": -1.0})),
        (
            "throughput",
            json!({"requests_per_second": -1.0, "output_tokens_per_second": null}),
        ),
        (
            "throughput",
            json!({"requests_per_second": 1.0, "output_tokens_per_second": -1.0}),
        ),
        ("ttft_seconds", json!({"p50": "NaN", "p95": 1.0})),
    ] {
        let mut value = evidence(&run);
        value["points"][0][field] = bad;
        assert!(parse(&run, &value).is_err(), "accepted bad {field}");
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        let mut result = parse(&run, &evidence(&run)).unwrap();
        result.points[0].ttft_seconds = Some(BenchmarkPercentiles { p50: bad, p95: 1.0 });
        assert!(result.validate_for(&run).is_err());
        let mut result = parse(&run, &evidence(&run)).unwrap();
        result.points[0].throughput.requests_per_second = bad;
        assert!(result.validate_for(&run).is_err());
        let mut result = parse(&run, &evidence(&run)).unwrap();
        result.points[0].throughput.output_tokens_per_second = Some(bad);
        assert!(result.validate_for(&run).is_err());
        let mut result = parse(&run, &evidence(&run)).unwrap();
        result.points[0].achieved_request_rate = Some(bad);
        assert!(result.validate_for(&run).is_err());
        let mut result = parse(&run, &evidence(&run)).unwrap();
        result.points[0]
            .goodput
            .as_mut()
            .unwrap()
            .requests_per_second = bad;
        assert!(result.validate_for(&run).is_err());
    }
    let mut result = parse(&run, &evidence(&run)).unwrap();
    result.points[0].itl_seconds = Some(BenchmarkPercentiles { p50: 2.0, p95: 1.0 });
    assert!(result.validate_for(&run).is_err());
}

#[test]
fn rejects_incomplete_stage_counts_and_inconsistent_missing_measurements() {
    let run = run();
    let mut value = evidence(&run);
    value["points"][0]["failures"] = json!(0);
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["points"][0]["goodput"] = Value::Null;
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["points"][1]["ttft_seconds"] = json!({"p50": 0.1, "p95": 0.2});
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["points"][1]["throughput"]["requests_per_second"] = json!(1.0);
    assert!(parse(&run, &value).is_err());
}

#[test]
fn optional_measurements_must_be_present_but_may_be_null() {
    let run = run();
    assert!(parse(&run, &evidence(&run)).is_ok());
    for field in [
        "achieved_request_rate",
        "ttft_seconds",
        "itl_seconds",
        "goodput",
    ] {
        let mut value = evidence(&run);
        value["points"][1].as_object_mut().unwrap().remove(field);
        assert!(parse(&run, &value).is_err(), "accepted missing {field}");
    }
    let mut value = evidence(&run);
    value["points"][1]["throughput"]
        .as_object_mut()
        .unwrap()
        .remove("output_tokens_per_second");
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["points"][1]["throughput"]
        .as_object_mut()
        .unwrap()
        .remove("requests_per_second");
    assert!(parse(&run, &value).is_err());
}

#[test]
fn points_stages_and_request_budgets_are_bounded_and_consistent() {
    let run = run();
    let mut value = evidence(&run);
    value["points"] = json!([]);
    value["definition"]["spec"]["load"]["stages"] = json!([]);
    let mut bounded_run = run.clone();
    bind_job(&mut bounded_run, &value);
    assert!(parse(&bounded_run, &value).is_err());

    value = evidence(&run);
    let point = value["points"][0].clone();
    let stage = value["definition"]["spec"]["load"]["stages"][0].clone();
    value["points"] = json!(vec![point; BENCHMARK_MAX_POINTS + 1]);
    value["definition"]["spec"]["load"]["stages"] = json!(vec![stage; BENCHMARK_MAX_POINTS + 1]);
    bind_job(&mut bounded_run, &value);
    assert!(parse(&bounded_run, &value).is_err());

    value = evidence(&run);
    value["points"][0]["stage"]["rate"] = json!(3);
    assert!(parse(&run, &value).is_err());
    value = evidence(&run);
    value["definition"]["spec"]["data"]["input_len"] = json!(0);
    bind_job(&mut bounded_run, &value);
    assert!(parse(&bounded_run, &value).is_err());
    value = evidence(&run);
    value["definition"]["spec"]["goodput"]["itl"] = json!(0);
    bind_job(&mut bounded_run, &value);
    assert!(parse(&bounded_run, &value).is_err());

    let mut too_few_requests = run.clone();
    too_few_requests.request.workload.max_requests = 14; // 14 measured requests plus one warmup
    assert!(parse(&too_few_requests, &evidence(&too_few_requests)).is_err());
    let mut too_short = run.clone();
    too_short.request.workload.duration_seconds = 5; // 5 stage seconds plus one warmup
    assert!(parse(&too_short, &evidence(&too_short)).is_err());
    let mut too_few_tokens = run.clone();
    too_few_tokens.request.workload.model_budget_tokens = 959; // 15 * 64 tokens
    assert!(parse(&too_few_tokens, &evidence(&too_few_tokens)).is_err());

    value = evidence(&run);
    value["definition"]["spec"]["load"] =
        json!({"type":"concurrent","stages":[{"concurrency_level":9,"num_requests":10}]});
    value["points"].as_array_mut().unwrap().truncate(1);
    value["points"][0]["stage"] =
        json!({"type":"concurrent","concurrency_level":9,"num_requests":10});
    value["points"][0]["successes"] = json!(9);
    let mut concurrent_run = run.clone();
    bind_job(&mut concurrent_run, &value);
    assert!(parse(&concurrent_run, &value).is_err());
    value["definition"]["spec"]["load"]["stages"][0]["concurrency_level"] = json!(8);
    value["points"][0]["stage"]["concurrency_level"] = json!(8);
    bind_job(&mut concurrent_run, &value);
    let parsed = parse(&concurrent_run, &value).unwrap();
    assert_eq!(
        parsed.points[0].stage,
        BenchmarkStage::Concurrent {
            concurrency_level: 8,
            num_requests: 10
        }
    );
}

#[test]
fn shared_prefix_and_multiturn_are_part_of_exact_workload_identity() {
    let mut first = run();
    let mut first_value = evidence(&first);
    first_value["definition"]["spec"]["data"] = json!({
        "type": "shared_prefix",
        "num_groups": 2,
        "num_prompts_per_group": 3,
        "system_prompt_len": 128,
        "question_len": 32,
        "output_len": 64,
        "enable_multi_turn_chat": true
    });
    bind_job(&mut first, &first_value);
    let first_result = parse(&first, &first_value).unwrap();
    let mut second = candidate();
    let mut second_value = evidence(&second);
    second_value["definition"]["spec"]["data"] = first_value["definition"]["spec"]["data"].clone();
    second_value["definition"]["spec"]["data"]["enable_multi_turn_chat"] = json!(false);
    bind_job(&mut second, &second_value);
    let second_result = parse(&second, &second_value).unwrap();
    assert!(compare_benchmark_results(&first, first_result, &second, second_result).is_err());
}

#[test]
fn comparison_rejects_changed_tenant_test_and_measurement_identity() {
    let baseline_run = run();
    let baseline = parse(&baseline_run, &evidence(&baseline_run)).unwrap();
    for change in 0..7 {
        let mut other = candidate();
        match change {
            0 => other.team = "team-b".into(),
            1 => other.request.project_id = Uuid::from_u128(101),
            2 => other.request.service = "different".into(),
            3 => other.request.runner = "another-template".into(),
            4 => other.request.workload.seed += 1,
            5 => other.request.workload.max_requests += 1,
            6 => other.request.evidence.test_definition = "sha256:other-test".into(),
            _ => unreachable!(),
        }
        let result = parse(&other, &evidence(&other)).unwrap();
        assert!(
            compare_benchmark_results(&baseline_run, baseline.clone(), &other, result).is_err(),
            "accepted identity change {change}"
        );
    }
    for change in 0..6 {
        let mut other = candidate();
        let mut value = evidence(&other);
        match change {
            0 => value["definition"]["spec"]["goodput"]["ttft"] = json!(0.6),
            1 => value["definition"]["spec"]["data"]["input_len"] = json!(129),
            2 => {
                value["definition"]["spec"]["load"]["stages"][0]["rate"] = json!(3);
                value["points"][0]["stage"]["rate"] = json!(3);
                value["points"][0]["successes"] = json!(8);
            }
            3 => value["tokenizer"]["repository"] = json!("other/tokenizer"),
            4 => value["tokenizer"]["revision"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            5 => value["runner"]["image"] = json!("example.org/adapter@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            _ => unreachable!(),
        }
        bind_job(&mut other, &value);
        let result = parse(&other, &value).unwrap();
        assert!(
            compare_benchmark_results(&baseline_run, baseline.clone(), &other, result).is_err(),
            "accepted definition change {change}"
        );
    }
    let other = candidate();
    // validate_for is repeated by comparison even when a caller mutates an already parsed result.
    let mut forged = parse(&other, &evidence(&other)).unwrap();
    forged.run_id = baseline_run.id;
    assert!(compare_benchmark_results(&baseline_run, baseline, &other, forged).is_err());
}

#[test]
fn comparison_exposes_both_curves_changes_and_timestamp_provenance() {
    let first = run();
    let mut second = candidate();
    second.request.deployment = "candidate".into();
    second.request.evidence.desired_configuration = "sha256:desired-b".into();
    second.request.evidence.observed_runtime = "sha256:runtime-b".into();
    second.request.evidence.conditions.cache_state = "cold".into();
    second.request.evidence.conditions.hardware = "gpu-b".into();
    second.request.evidence.conditions.observed_at = Utc.timestamp_opt(200, 0).unwrap();
    let first_result = parse(&first, &evidence(&first)).unwrap();
    let mut second_value = evidence(&second);
    second_value["points"][0]["throughput"]["requests_per_second"] = json!(2.1);
    let second_result = parse(&second, &second_value).unwrap();
    let compared =
        compare_benchmark_results(&first, first_result.clone(), &second, second_result.clone())
            .unwrap();
    assert_eq!(compared.baseline.points, first_result.points);
    assert_eq!(compared.candidate.points, second_result.points);
    assert_eq!(
        compared.baseline.points[0].throughput.requests_per_second,
        1.5
    );
    assert_eq!(
        compared.candidate.points[0].throughput.requests_per_second,
        2.1
    );
    assert!(compared.desired_configuration.changed);
    assert_eq!(compared.desired_configuration.baseline, "sha256:desired-a");
    assert_eq!(compared.desired_configuration.candidate, "sha256:desired-b");
    assert!(compared.observed_runtime.changed);
    assert_eq!(compared.observed_runtime.baseline, "sha256:runtime-a");
    assert_eq!(compared.observed_runtime.candidate, "sha256:runtime-b");
    assert!(compared.conditions.changed);
    assert_eq!(compared.conditions.baseline.cache_state, "warm");
    assert_eq!(compared.conditions.candidate.cache_state, "cold");
    assert!(compared.deployment.changed);
    assert!(compared.observed_at.changed);
    assert_eq!(
        compared.observed_at.baseline,
        first.request.evidence.conditions.observed_at
    );
    assert_eq!(
        compared.observed_at.candidate,
        second.request.evidence.conditions.observed_at
    );
    let serialized = serde_json::to_value(&compared).unwrap();
    assert!(serialized.get("score").is_none());
    assert_eq!(
        serialized["candidate"]["points"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn every_non_timestamp_condition_is_compared() {
    let first = run();
    let first_result = parse(&first, &evidence(&first)).unwrap();
    for field in 0..5 {
        let mut second = candidate();
        let conditions = &mut second.request.evidence.conditions;
        match field {
            0 => conditions.hardware = "gpu-b".into(),
            1 => conditions.nodes = vec!["node-b".into()],
            2 => conditions.replicas = 2,
            3 => conditions.cache_state = "cold".into(),
            4 => conditions.competing_traffic = "high".into(),
            _ => unreachable!(),
        }
        let second_result = parse(&second, &evidence(&second)).unwrap();
        let compared =
            compare_benchmark_results(&first, first_result.clone(), &second, second_result)
                .unwrap();
        assert!(compared.conditions.changed, "condition {field} was ignored");
        assert!(!compared.observed_at.changed);
    }
}

#[test]
fn timestamps_alone_are_provenance_not_changed_conditions() {
    let first = run();
    let mut second = candidate();
    second.request.evidence.conditions.observed_at = Utc.timestamp_opt(200, 0).unwrap();
    let first_result = parse(&first, &evidence(&first)).unwrap();
    let second_result = parse(&second, &evidence(&second)).unwrap();
    let compared = compare_benchmark_results(&first, first_result, &second, second_result).unwrap();
    assert!(!compared.conditions.changed);
    assert!(!compared.desired_configuration.changed);
    assert!(!compared.observed_runtime.changed);
    assert!(compared.observed_at.changed);
    assert_ne!(
        compared.conditions.baseline.observed_at,
        compared.conditions.candidate.observed_at
    );
}
