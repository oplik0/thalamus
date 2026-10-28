use crate::{Error, Result, Run, RunKind};
use k8s_openapi::api::batch::v1::Job;
use serde_json::json;
use std::collections::BTreeMap;

/// Trusted operator configuration, not an API request. Commands and images must
/// be supplied by administrators and images must be pinned by SHA-256 digest.
#[derive(Clone, Debug)]
pub struct JobTemplate {
    pub kind: RunKind,
    pub image: String,
    pub command: Vec<String>,
    pub workloads: Vec<String>,
    /// Debug accounts must be namespace-scoped and read-only.
    pub service_account: String,
    /// Optional secret-name prefix; the worker provisions `<prefix>-<run UUID>`
    /// with a short-lived key and removes it after the run.
    pub key_secret: Option<String>,
}

/// Operator-provided upload destination. The worker signs a short-lived PUT
/// for the exact result object and places it in an owned per-run Secret.
#[derive(Clone, Debug)]
pub struct ResultUpload {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub secret_prefix: String,
}

fn dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

impl ResultUpload {
    fn validate(&self) -> Result<()> {
        let endpoint = url::Url::parse(&self.endpoint)
            .map_err(|_| Error::Invalid("invalid result endpoint".into()))?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
            || self.endpoint.len() > 1024
            || self.bucket.len() < 3
            || !dns_label(&self.bucket, 63)
            || !dns_label(&self.region, 63)
            || !dns_label(&self.secret_prefix, 26)
        {
            return Err(Error::Invalid("invalid result upload configuration".into()));
        }
        Ok(())
    }
}
// Both runner configurations are passed as single environment strings.
const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn benchmark_fields<'a>(
    value: &'a serde_json::Value,
    expected: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Invalid("invalid benchmark configuration".into()))?;
    if object.len() != expected.len() || object.keys().any(|key| !expected.contains(&key.as_str()))
    {
        return Err(Error::Invalid("invalid benchmark configuration".into()));
    }
    Ok(object)
}

fn benchmark_integer(value: &serde_json::Value, max: u64) -> Result<u64> {
    value
        .as_u64()
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| Error::Invalid("invalid benchmark configuration".into()))
}

fn benchmark_repository(repository: &str) -> bool {
    if repository.len() > 256 {
        return false;
    }
    let mut segments = repository.split('/');
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && !segment.starts_with(['.', '-'])
            && !segment.ends_with(['.', '-'])
            && !segment.contains("..")
            && !segment.contains("--")
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    valid_segment(segments.next().unwrap_or(""))
        && segments.next().is_none_or(valid_segment)
        && segments.next().is_none()
}

/// Validate operator-approved inference-perf workload options before target binding.
pub fn validate_benchmark_options(value: &serde_json::Value) -> Result<()> {
    let root = benchmark_fields(value, &["tokenizer", "workloads"])?;
    let tokenizer = benchmark_fields(&root["tokenizer"], &["repository", "revision"])?;
    if !tokenizer["repository"]
        .as_str()
        .is_some_and(benchmark_repository)
        || !tokenizer["revision"].as_str().is_some_and(|revision| {
            revision.len() == 40
                && revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(Error::Invalid("invalid benchmark tokenizer".into()));
    }
    let workloads = root["workloads"]
        .as_object()
        .filter(|workloads| (1..=128).contains(&workloads.len()))
        .ok_or_else(|| Error::Invalid("invalid benchmark workloads".into()))?;
    for (name, workload) in workloads {
        crate::text(name, "benchmark workload", 128)?;
        if name.trim().is_empty() {
            return Err(Error::Invalid("invalid benchmark workload".into()));
        }
        let workload = benchmark_fields(workload, &["load", "data", "goodput"])?;
        let load = benchmark_fields(&workload["load"], &["type", "stages"])?;
        let stages = load["stages"]
            .as_array()
            .filter(|stages| (1..=16).contains(&stages.len()))
            .ok_or_else(|| Error::Invalid("invalid benchmark stages".into()))?;
        for stage in stages {
            match load["type"].as_str() {
                Some("constant") => {
                    let stage = benchmark_fields(stage, &["rate", "duration"])?;
                    benchmark_integer(&stage["rate"], 256)?;
                    benchmark_integer(&stage["duration"], 3600)?;
                }
                Some("concurrent") => {
                    let stage = benchmark_fields(stage, &["concurrency_level", "num_requests"])?;
                    benchmark_integer(&stage["concurrency_level"], 256)?;
                    benchmark_integer(&stage["num_requests"], 1_000_000)?;
                }
                _ => return Err(Error::Invalid("invalid benchmark load type".into())),
            }
        }
        match workload["data"]
            .get("type")
            .and_then(|value| value.as_str())
        {
            Some("random") => {
                let data =
                    benchmark_fields(&workload["data"], &["type", "input_len", "output_len"])?;
                benchmark_integer(&data["input_len"], 65_536)?;
                benchmark_integer(&data["output_len"], 16_384)?;
            }
            Some("shared_prefix") => {
                let data = benchmark_fields(
                    &workload["data"],
                    &[
                        "type",
                        "num_groups",
                        "num_prompts_per_group",
                        "system_prompt_len",
                        "question_len",
                        "output_len",
                        "enable_multi_turn_chat",
                    ],
                )?;
                let groups = benchmark_integer(&data["num_groups"], 128)?;
                let prompts = benchmark_integer(&data["num_prompts_per_group"], 128)?;
                let system = benchmark_integer(&data["system_prompt_len"], 65_536)?;
                let question = benchmark_integer(&data["question_len"], 65_536)?;
                if groups * prompts * (system + question) > 4_000_000 {
                    return Err(Error::Invalid(
                        "benchmark data exceeds materialization limit".into(),
                    ));
                }
                benchmark_integer(&data["output_len"], 16_384)?;
                if !data["enable_multi_turn_chat"].is_boolean() {
                    return Err(Error::Invalid("invalid benchmark data".into()));
                }
            }
            _ => return Err(Error::Invalid("invalid benchmark data type".into())),
        }
        let goodput = benchmark_fields(&workload["goodput"], &["ttft", "itl"])?;
        for field in ["ttft", "itl"] {
            if !goodput[field]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n > 0.0 && n <= 3600.0)
            {
                return Err(Error::Invalid("invalid benchmark goodput".into()));
            }
        }
    }
    let mut size = ByteCounter::default();
    serde_json::to_writer(&mut size, value)?;
    if size.0 > MAX_CONFIG_BYTES {
        return Err(Error::Invalid(
            "benchmark options exceed environment limit".into(),
        ));
    }
    Ok(())
}

fn validate_stage_memory(data: &serde_json::Value, count: u64) -> Result<()> {
    let number = |field: &str| data[field].as_u64().expect("validated benchmark data");
    let output = number("output_len");
    let per_request = if data["type"].as_str() == Some("random") {
        number("input_len") + output
    } else {
        let system = number("system_prompt_len");
        let question = number("question_len");
        let mut tokens = 2 * system + question + output;
        if data["enable_multi_turn_chat"].as_bool() == Some(true) {
            let sessions = number("num_groups") * number("num_prompts_per_group");
            let previous_turns = (count - 1) / sessions;
            tokens += previous_turns * (system + question + output);
        }
        tokens
    };
    if count * per_request > 16_000_000 {
        return Err(Error::Invalid(
            "benchmark stage exceeds token memory limit".into(),
        ));
    }
    Ok(())
}

/// Validate workload budgets and per-stage memory using the inference-perf adapter's rules.
pub fn validate_benchmark_workload(
    options: &serde_json::Value,
    workload: &crate::Workload,
) -> Result<()> {
    validate_benchmark_options(options)?;
    crate::text(&workload.name, "benchmark workload", 128)?;
    if workload.name.trim().is_empty()
        || !(1..=256).contains(&workload.concurrency)
        || !(1..=3600).contains(&workload.duration_seconds)
        || workload.warmup_seconds > workload.duration_seconds
        || !(1..=1_000_000).contains(&workload.max_requests)
        || !(1..=1_000_000).contains(&workload.model_budget_tokens)
        || workload.tool_budget_calls > 1000
    {
        return Err(Error::Invalid(
            "benchmark workload outside permitted bounds".into(),
        ));
    }
    let spec = options["workloads"]
        .get(workload.name.as_str())
        .ok_or_else(|| Error::Invalid("benchmark workload is not configured".into()))?;
    let load = &spec["load"];
    let data = &spec["data"];
    let stages = load["stages"]
        .as_array()
        .expect("validated benchmark stages");
    let constant = load["type"].as_str() == Some("constant");
    let warmup = u64::from(workload.warmup_seconds);
    let mut total_requests = warmup;
    let mut total_duration = warmup;
    if warmup > 0 {
        validate_stage_memory(data, warmup)?;
    }
    for stage in stages {
        let count = if constant {
            let rate = stage["rate"].as_u64().expect("validated stage rate");
            let duration = stage["duration"]
                .as_u64()
                .expect("validated stage duration");
            total_duration += duration;
            rate * duration
        } else {
            let concurrency = stage["concurrency_level"]
                .as_u64()
                .expect("validated stage concurrency");
            if concurrency > u64::from(workload.concurrency) {
                return Err(Error::Invalid(
                    "benchmark stage exceeds concurrency budget".into(),
                ));
            }
            stage["num_requests"]
                .as_u64()
                .expect("validated request count")
        };
        total_requests += count;
        validate_stage_memory(data, count)?;
    }
    if (constant && total_duration > u64::from(workload.duration_seconds))
        || total_requests > u64::from(workload.max_requests)
        || total_requests
            * data["output_len"]
                .as_u64()
                .expect("validated output length")
            > u64::from(workload.model_budget_tokens)
    {
        return Err(Error::Invalid(
            "benchmark stages exceed workload budget".into(),
        ));
    }
    Ok(())
}

fn validate_target(object: &serde_json::Map<String, serde_json::Value>) -> Result<(&str, &str)> {
    let base_text = object
        .get("base_url")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Invalid("invalid runner base URL".into()))?;
    let base =
        url::Url::parse(base_text).map_err(|_| Error::Invalid("invalid runner base URL".into()))?;
    let authority = base_text
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next());
    if base_text.len() > 1024
        || base_text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || authority.is_none_or(|authority| authority.is_empty() || authority.contains(['@', '\\']))
    {
        return Err(Error::Invalid("invalid runner base URL".into()));
    }
    let model = object
        .get("model")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Invalid("invalid runner model".into()))?;
    crate::text(model, "runner model", 256)?;
    if model.trim().is_empty() {
        return Err(Error::Invalid("invalid runner model".into()));
    }
    Ok((base_text, model))
}

fn validate_validation_fields(config: &serde_json::Value) -> Result<()> {
    let object = config
        .as_object()
        .ok_or_else(|| Error::Invalid("validation config must be an object".into()))?;
    let allowed = [
        "base_url",
        "model",
        "forbidden_model",
        "suite",
        "backend_header",
        "expected_backend",
        "old_backend",
        "context_prompt",
        "context_tokens",
        "tool_parser",
        "reasoning",
        "responses",
        "anthropic",
        "responses_continuation",
        "timeout_seconds",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Invalid("unknown validation config field".into()));
    }
    validate_target(object)?;
    Ok(())
}

/// Validate a resolved validation configuration without allocating its encoding.
pub fn validate_observed_validation_config(config: &serde_json::Value) -> Result<()> {
    validate_validation_fields(config)?;
    let mut size = ByteCounter::default();
    serde_json::to_writer(&mut size, config)?;
    if size.0 > MAX_CONFIG_BYTES {
        return Err(Error::Invalid(
            "validation config exceeds environment limit".into(),
        ));
    }
    Ok(())
}

fn encode_validation_config(config: &serde_json::Value) -> Result<String> {
    validate_validation_fields(config)?;
    let json = serde_json::to_string(config)?;
    if json.len() > MAX_CONFIG_BYTES {
        return Err(Error::Invalid(
            "validation config exceeds environment limit".into(),
        ));
    }
    Ok(json)
}

fn encode_benchmark_config(
    config: &serde_json::Value,
    workload: &crate::Workload,
) -> Result<String> {
    let object = benchmark_fields(config, &["base_url", "model", "benchmark"])?;
    let (base_url, _) = validate_target(object)?;
    if !base_url.ends_with("/v1") {
        return Err(Error::Invalid("invalid benchmark base URL".into()));
    }
    validate_benchmark_workload(&object["benchmark"], workload)?;
    let json = serde_json::to_string(config)?;
    if json.len() > MAX_CONFIG_BYTES {
        return Err(Error::Invalid(
            "benchmark config exceeds environment limit".into(),
        ));
    }
    Ok(json)
}

#[derive(Clone, Debug)]
pub struct Templates {
    namespace: String,
    runners: BTreeMap<String, JobTemplate>,
    result_upload: ResultUpload,
}

impl Templates {
    pub fn new(
        namespace: String,
        runners: BTreeMap<String, JobTemplate>,
        result_upload: ResultUpload,
    ) -> Result<Self> {
        result_upload.validate()?;
        crate::text(&namespace, "namespace", 253)?;
        if runners.is_empty() {
            return Err(Error::Invalid("no runner templates configured".into()));
        }
        for (name, t) in &runners {
            crate::text(name, "runner", 128)?;
            crate::text(&t.service_account, "service account", 253)?;
            let (image, digest) = t
                .image
                .rsplit_once("@sha256:")
                .ok_or_else(|| Error::Invalid("runner image must be digest pinned".into()))?;
            if image.is_empty()
                || digest.len() != 64
                || !digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(Error::Invalid("invalid runner image digest".into()));
            }
            if matches!(t.kind, RunKind::Benchmark)
                && (t.image.len() > 2048
                    || image.contains("://")
                    || image.chars().any(|c| {
                        c.is_whitespace() || c.is_control() || matches!(c, '@' | '?' | '#' | '\\')
                    }))
            {
                return Err(Error::Invalid("invalid benchmark runner image".into()));
            }
            if t.command.is_empty()
                || t.command.len() > 32
                || t.workloads.is_empty()
                || t.workloads.len() > 128
            {
                return Err(Error::Invalid("invalid runner template".into()));
            }
            for s in t.command.iter().chain(t.workloads.iter()) {
                crate::text(s, "template entry", 1024)?;
            }
            if let Some(s) = &t.key_secret {
                crate::text(s, "key secret", 253)?;
            }
        }
        Ok(Self {
            namespace,
            runners,
            result_upload,
        })
    }

    /// Whether the functional validation runner is explicitly configured and keyed.
    pub fn supports_validation_runner(&self, runner: &str, workload: &str) -> bool {
        workload == "functional"
            && self.runners.get(runner).is_some_and(|template| {
                template.kind == RunKind::Validation
                    && template.key_secret.is_some()
                    && template.workloads.iter().any(|allowed| allowed == workload)
            })
    }
}

/// Constructs a typed Job from the trusted runner, named workload and resolved
/// configuration. The caller must authorize the run and independently derive
/// the configuration from current observations and approved test options.
/// The runner receives its presigned PUT URL only via a per-run Secret.
pub fn build_job(run: &Run, templates: &Templates, config: &serde_json::Value) -> Result<Job> {
    run.request.validate()?;
    if matches!(run.request.kind, RunKind::Debug) {
        return Err(Error::Invalid(
            "debug runs require the dedicated debug reconciler".into(),
        ));
    }
    if run.fingerprint != run.request.fingerprint()? || run.job_name != crate::job_name(run.id) {
        return Err(Error::Invalid("run identity mismatch".into()));
    }
    let template = templates
        .runners
        .get(&run.request.runner)
        .ok_or_else(|| Error::Invalid("unknown runner".into()))?;
    if template.kind != run.request.kind || !template.workloads.contains(&run.request.workload.name)
    {
        return Err(Error::Invalid("runner or workload not permitted".into()));
    }
    if template.key_secret.is_some() && run.request.workload.model_budget_tokens == 0 {
        return Err(Error::Invalid(
            "runner key requires a positive model token budget".into(),
        ));
    }
    let mut env = vec![
        json!({"name":"THALAMUS_RUN_ID", "value":run.id.to_string()}),
        json!({"name":"THALAMUS_RUN_REQUEST", "value":serde_json::to_string(&run.request)?}),
        json!({"name":"THALAMUS_S3_CA_FILE", "value":"/var/run/thalamus/result-upload/ca.pem"}),
    ];
    if matches!(run.request.kind, RunKind::Validation) {
        if template.key_secret.is_none() {
            return Err(Error::Invalid(
                "validation runner requires a run key secret".into(),
            ));
        }
        let config = encode_validation_config(config)?;
        env.push(json!({"name":"THALAMUS_VALIDATION_CONFIG","value":config}));
    }
    if matches!(run.request.kind, RunKind::Benchmark) {
        if template.key_secret.is_none() {
            return Err(Error::Invalid(
                "benchmark runner requires a run key secret".into(),
            ));
        }
        let config = encode_benchmark_config(config, &run.request.workload)?;
        env.push(json!({"name":"THALAMUS_BENCHMARK_CONFIG","value":config}));
        env.push(json!({"name":"THALAMUS_RUNNER_IMAGE","value":template.image}));
        env.push(json!({"name":"HOME","value":"/tmp"}));
        env.push(json!({"name":"HF_HOME","value":"/tmp"}));
    }
    let secret = format!("{}-{}", templates.result_upload.secret_prefix, run.id);
    if let Some(secret) = &template.key_secret {
        env.push(json!({"name":"THALAMUS_API_KEY","valueFrom":{"secretKeyRef":{"name":format!("{secret}-{}", run.id),"key":"key"}}}));
    }
    let deadline = u64::from(run.request.workload.duration_seconds) + 300;
    let mut value = json!({
        "apiVersion":"batch/v1", "kind":"Job",
        "metadata":{"name":run.job_name,"namespace":templates.namespace,
            "annotations":{"thalamus.io/ownership-nonce":run.ownership_nonce.to_string()},
            "labels":{"app.kubernetes.io/managed-by":"thalamus","thalamus.io/run-id":run.id.to_string()}},
        "spec":{"backoffLimit":0,"activeDeadlineSeconds":deadline,"ttlSecondsAfterFinished":3600,
            "template":{"metadata":{"labels":{"app.kubernetes.io/managed-by":"thalamus","thalamus.io/run-id":run.id.to_string()}},
                "spec":{"restartPolicy":"Never","serviceAccountName":template.service_account,
                    "automountServiceAccountToken":false,
                    "securityContext":{"runAsNonRoot":true,"runAsUser":65532,"runAsGroup":65532,"seccompProfile":{"type":"RuntimeDefault"}},
                    "volumes":[{"name":"result-upload","projected":{"defaultMode":292,"sources":[{"secret":{"name":secret,"items":[{"key":"url","path":"url"}]}},{"secret":{"name":secret,"optional":true,"items":[{"key":"ca.crt","path":"ca.pem"}]}}]}}],
                    "containers":[{"name":"runner","image":template.image,"command":template.command,"env":env,
                        "volumeMounts":[{"name":"result-upload","mountPath":"/var/run/thalamus/result-upload","readOnly":true}],
                        "resources":{"requests":{"cpu":"100m","memory":"128Mi","ephemeral-storage":"64Mi"},"limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"1Gi"}},
                        "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}}}]}}}
    });
    if matches!(template.kind, RunKind::Benchmark) {
        let pod = &mut value["spec"]["template"]["spec"];
        // The non-root runner needs write access to the bounded tokenizer/report scratch space.
        pod["securityContext"]["fsGroup"] = json!(65532);
        pod["volumes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"benchmark-tmp","emptyDir":{"sizeLimit":"512Mi"}}));
        pod["containers"][0]["volumeMounts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"benchmark-tmp","mountPath":"/tmp"}));
    }
    Ok(serde_json::from_value(value)?)
}

/// Verify a CREATE conflict before recovery. This requires namespace RBAC and
/// admission to reserve the thalamus-run- prefix, ownership annotation and
/// managed-by label for Thalamus; a principal that can forge those can adopt a
/// run. The prepared spec is persisted before issuing CREATE.
pub fn verify_recoverable_job(run: &Run, job: &Job) -> Result<()> {
    let uid = job.metadata.uid.as_deref().ok_or(Error::ForeignJob)?;
    let labels = job.metadata.labels.as_ref().ok_or(Error::ForeignJob)?;
    let annotations = job.metadata.annotations.as_ref().ok_or(Error::ForeignJob)?;
    if run.job_spec.is_none()
        || job.metadata.name.as_deref() != Some(run.job_name.as_str())
        || job.metadata.namespace.as_deref() != run.job_namespace.as_deref()
        || labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            != Some("thalamus")
        || labels.get("thalamus.io/run-id").map(String::as_str) != Some(run.id.to_string().as_str())
        || annotations
            .get("thalamus.io/ownership-nonce")
            .map(String::as_str)
            != Some(run.ownership_nonce.to_string().as_str())
        || run.job_uid.as_deref().is_some_and(|old| old != uid)
    {
        return Err(Error::ForeignJob);
    }
    let actual = serde_json::to_value(job.spec.as_ref().ok_or(Error::ForeignJob)?)?;
    if normalize_spec(actual, uid, &run.job_name)?
        != normalize_spec(
            run.job_spec.clone().ok_or(Error::ForeignJob)?,
            uid,
            &run.job_name,
        )?
    {
        return Err(Error::ForeignJob);
    }
    Ok(())
}

fn remove_default(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
) {
    if obj.get(key) == Some(&value) {
        obj.remove(key);
    }
}

fn normalize_spec(mut spec: serde_json::Value, uid: &str, name: &str) -> Result<serde_json::Value> {
    let top = spec.as_object_mut().ok_or(Error::ForeignJob)?;
    if let Some(selector) = top.remove("selector") {
        let labels = selector
            .get("matchLabels")
            .and_then(|v| v.as_object())
            .ok_or(Error::ForeignJob)?;
        if labels.is_empty()
            || labels.iter().any(|(k, v)| {
                !matches!(
                    k.as_str(),
                    "controller-uid" | "batch.kubernetes.io/controller-uid"
                ) || v.as_str() != Some(uid)
            })
        {
            return Err(Error::ForeignJob);
        }
    }
    for (k, v) in [
        ("manualSelector", json!(false)),
        ("parallelism", json!(1)),
        ("completions", json!(1)),
        ("completionMode", json!("NonIndexed")),
        ("suspend", json!(false)),
    ] {
        remove_default(top, k, v);
    }
    let template = top
        .get_mut("template")
        .and_then(|v| v.as_object_mut())
        .ok_or(Error::ForeignJob)?;
    if let Some(labels) = template
        .get_mut("metadata")
        .and_then(|v| v.get_mut("labels"))
        .and_then(|v| v.as_object_mut())
    {
        for (key, expected) in [
            ("controller-uid", uid),
            ("batch.kubernetes.io/controller-uid", uid),
            ("job-name", name),
            ("batch.kubernetes.io/job-name", name),
        ] {
            if let Some(value) = labels.get(key) {
                if value.as_str() != Some(expected) {
                    return Err(Error::ForeignJob);
                }
                labels.remove(key);
            }
        }
    }
    let pod = template
        .get_mut("spec")
        .and_then(|v| v.as_object_mut())
        .ok_or(Error::ForeignJob)?;
    for (k, v) in [
        ("dnsPolicy", json!("ClusterFirst")),
        ("schedulerName", json!("default-scheduler")),
        ("terminationGracePeriodSeconds", json!(30)),
        ("enableServiceLinks", json!(true)),
        ("preemptionPolicy", json!("PreemptLowerPriority")),
    ] {
        remove_default(pod, k, v);
    }
    if pod.get("serviceAccount") == pod.get("serviceAccountName") {
        pod.remove("serviceAccount");
    }
    let containers = pod
        .get_mut("containers")
        .and_then(|v| v.as_array_mut())
        .ok_or(Error::ForeignJob)?;
    for container in containers {
        let c = container.as_object_mut().ok_or(Error::ForeignJob)?;
        for (k, v) in [
            ("imagePullPolicy", json!("IfNotPresent")),
            ("terminationMessagePath", json!("/dev/termination-log")),
            ("terminationMessagePolicy", json!("File")),
        ] {
            remove_default(c, k, v);
        }
    }
    Ok(spec)
}

pub fn verify_owned_job(run: &Run, job: &Job, namespace: &str) -> Result<()> {
    if run.job_uid.is_none() || run.job_namespace.as_deref() != Some(namespace) {
        return Err(Error::ForeignJob);
    }
    verify_recoverable_job(run, job)
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn upload_configuration_rejects_credentials_and_unbounded_secret_names() {
        let mut upload = ResultUpload {
            endpoint: "https://objects.example".into(),
            bucket: "run-results".into(),
            region: "us-east-1".into(),
            secret_prefix: "upload".into(),
        };
        assert!(upload.validate().is_ok());
        upload.endpoint = "https://user:password@objects.example".into();
        assert!(upload.validate().is_err());
        upload.endpoint = "http://objects.example".into();
        assert!(upload.validate().is_err());
        upload.endpoint = "https://objects.example".into();
        upload.secret_prefix = "a".repeat(27);
        assert!(upload.validate().is_err());
    }
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;
    use crate::{EvidenceIdentity, RunConditions, RunRequest, Workload};
    use chrono::{TimeZone, Utc};
    use serde_json::Value;
    use uuid::Uuid;

    fn run(kind: RunKind) -> Run {
        let workload = if kind == RunKind::Validation {
            "functional"
        } else {
            "load"
        };
        let request = RunRequest {
            project_id: Uuid::from_u128(100),
            kind,
            service: "service-a".into(),
            deployment: "deployment-a".into(),
            runner: "runner".into(),
            workload: Workload {
                name: workload.into(),
                seed: 1,
                concurrency: 1,
                duration_seconds: 60,
                warmup_seconds: 0,
                max_requests: 100,
                model_budget_tokens: 10_000,
                tool_budget_calls: 0,
            },
            evidence: EvidenceIdentity {
                desired_configuration: "desired".into(),
                observed_runtime: "runtime".into(),
                test_definition: "test".into(),
                conditions: RunConditions {
                    hardware: "gpu".into(),
                    nodes: vec!["node".into()],
                    replicas: 1,
                    cache_state: "warm".into(),
                    competing_traffic: "low".into(),
                    observed_at: Utc.timestamp_opt(100, 0).unwrap(),
                },
            },
        };
        let id = Uuid::from_u128(1);
        let now = Utc.timestamp_opt(100, 0).unwrap();
        Run {
            id,
            team: "team".into(),
            actor: "user".into(),
            idempotency_key: "retry".into(),
            fingerprint: request.fingerprint().unwrap(),
            request,
            job_name: crate::job_name(id),
            job_uid: None,
            ownership_nonce: Uuid::from_u128(2),
            job_spec: None,
            job_namespace: None,
            state: "pending".into(),
            artifacts: None,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn templates(kind: RunKind, key_secret: Option<&str>) -> Templates {
        Templates::new(
            "inference".into(),
            BTreeMap::from([(
                "runner".into(),
                JobTemplate {
                    kind,
                    image: format!("example/runner@sha256:{}", "a".repeat(64)),
                    command: vec!["/runner".into()],
                    workloads: vec!["functional".into(), "load".into()],
                    service_account: "runner".into(),
                    key_secret: key_secret.map(str::to_owned),
                },
            )]),
            ResultUpload {
                endpoint: "https://objects.example".into(),
                bucket: "run-results".into(),
                region: "us-east-1".into(),
                secret_prefix: "upload".into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn generic_job_builder_rejects_debug_runs() {
        let mut debug = run(RunKind::Benchmark);
        debug.request.kind = RunKind::Debug;
        debug.request.workload.tool_budget_calls = 1;
        debug.fingerprint = debug.request.fingerprint().unwrap();
        let error =
            build_job(&debug, &templates(RunKind::Debug, Some("api")), &json!({})).unwrap_err();
        assert!(error.to_string().contains("dedicated debug reconciler"));
    }

    fn validation_config() -> Value {
        json!({
            "base_url": "https://candidate.example/candidates/deployment-a/v1",
            "model": "org/model",
            "forbidden_model": "not-allowed",
            "suite": "candidate",
            "backend_header": "x-thalamus-backend",
            "expected_backend": "service-a/deployment-a/revision",
            "old_backend": "service-a/old/revision",
            "context_prompt": "test",
            "context_tokens": 1,
            "tool_parser": null,
            "reasoning": false,
            "responses": true,
            "anthropic": false,
            "responses_continuation": false,
            "timeout_seconds": 30
        })
    }

    fn benchmark_config() -> Value {
        json!({
            "base_url": "https://candidate.example/candidates/deployment-a/v1",
            "model": "org/model",
            "benchmark": {
                "tokenizer": {
                    "repository": "org/tokenizer",
                    "revision": "0123456789abcdef0123456789abcdef01234567"
                },
                "workloads": {
                    "load": {
                        "load": {
                            "type": "constant",
                            "stages": [{"rate": 1, "duration": 10}]
                        },
                        "data": {"type": "random", "input_len": 16, "output_len": 8},
                        "goodput": {"ttft": 1.0, "itl": 0.1}
                    }
                }
            }
        })
    }

    fn env<'a>(job: &'a Value, name: &str) -> Option<&'a Value> {
        job["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
    }

    #[test]
    fn benchmark_job_injects_resolved_config_and_is_bounded() {
        let run = run(RunKind::Benchmark);
        let config = benchmark_config();
        let templates = templates(RunKind::Benchmark, Some("api"));
        let job = serde_json::to_value(build_job(&run, &templates, &config).unwrap()).unwrap();
        let encoded = env(&job, "THALAMUS_BENCHMARK_CONFIG").unwrap()["value"]
            .as_str()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(encoded).unwrap(), config);
        assert!(env(&job, "THALAMUS_VALIDATION_CONFIG").is_none());
        assert_eq!(
            env(&job, "THALAMUS_RUNNER_IMAGE").unwrap()["value"],
            job["spec"]["template"]["spec"]["containers"][0]["image"]
        );
        assert_eq!(
            env(&job, "THALAMUS_API_KEY").unwrap()["valueFrom"]["secretKeyRef"],
            json!({"name":format!("api-{}", run.id), "key":"key"})
        );
        assert_eq!(env(&job, "HOME").unwrap()["value"], "/tmp");
        assert_eq!(env(&job, "HF_HOME").unwrap()["value"], "/tmp");
        assert_eq!(
            env(&job, "THALAMUS_S3_CA_FILE").unwrap()["value"],
            "/var/run/thalamus/result-upload/ca.pem"
        );
        assert!(env(&job, "SSL_CERT_FILE").is_none());
        assert_eq!(job["spec"]["activeDeadlineSeconds"], 360);
        assert_eq!(job["spec"]["backoffLimit"], 0);
        let pod = &job["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        assert_eq!(pod["securityContext"]["runAsUser"], 65532);
        assert_eq!(pod["securityContext"]["fsGroup"], 65532);
        assert_eq!(
            pod["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        let runner = &pod["containers"][0];
        assert_eq!(runner["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(runner["securityContext"]["allowPrivilegeEscalation"], false);
        assert_eq!(
            runner["securityContext"]["capabilities"]["drop"],
            json!(["ALL"])
        );
        assert_eq!(runner["resources"]["limits"]["ephemeral-storage"], "1Gi");
        assert_eq!(runner["resources"]["limits"]["memory"], "2Gi");
        let volumes = pod["volumes"].as_array().unwrap();
        assert!(volumes
            .iter()
            .any(|v| { v["name"] == "benchmark-tmp" && v["emptyDir"]["sizeLimit"] == "512Mi" }));
        let result_upload = volumes
            .iter()
            .find(|v| v["name"] == "result-upload")
            .unwrap();
        let sources = result_upload["projected"]["sources"].as_array().unwrap();
        assert_eq!(sources[0]["secret"]["name"], format!("upload-{}", run.id));
        assert_ne!(sources[0]["secret"]["optional"], true);
        assert_eq!(sources[0]["secret"]["items"][0]["key"], "url");
        assert_eq!(sources[1]["secret"]["name"], format!("upload-{}", run.id));
        assert_eq!(sources[1]["secret"]["optional"], true);
        assert_eq!(sources[1]["secret"]["items"][0]["key"], "ca.crt");
        assert_eq!(sources[1]["secret"]["items"][0]["path"], "ca.pem");
        let mounts = runner["volumeMounts"].as_array().unwrap();
        assert!(mounts.iter().any(|m| {
            m["name"] == "benchmark-tmp" && m["mountPath"] == "/tmp" && m["readOnly"] != true
        }));
        assert!(mounts.iter().any(|m| {
            m["name"] == "result-upload"
                && m["mountPath"] == "/var/run/thalamus/result-upload"
                && m["readOnly"] == true
        }));
    }

    #[test]
    fn validation_receives_only_its_unchanged_resolved_config() {
        let run = run(RunKind::Validation);
        let config = validation_config();
        let templates = templates(RunKind::Validation, Some("api"));
        let job = serde_json::to_value(build_job(&run, &templates, &config).unwrap()).unwrap();
        let encoded = env(&job, "THALAMUS_VALIDATION_CONFIG").unwrap()["value"]
            .as_str()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(encoded).unwrap(), config);
        assert!(env(&job, "THALAMUS_BENCHMARK_CONFIG").is_none());
        assert!(env(&job, "THALAMUS_RUNNER_IMAGE").is_none());
        assert!(env(&job, "HOME").is_none());
        assert!(env(&job, "HF_HOME").is_none());
        assert!(env(&job, "SSL_CERT_FILE").is_none());
        let pod = &job["spec"]["template"]["spec"];
        assert!(pod["securityContext"]["fsGroup"].is_null());
        assert!(!pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "benchmark-tmp"));
        assert_eq!(
            pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
            true
        );
    }

    #[test]
    fn benchmark_requires_config_key_budget_and_permitted_runner_workload() {
        let mut run = run(RunKind::Benchmark);
        let configured = templates(RunKind::Benchmark, Some("api"));
        let config = benchmark_config();
        for missing in [json!(null), json!({})] {
            assert!(build_job(&run, &configured, &missing).is_err());
        }
        let without_key = templates(RunKind::Benchmark, None);
        assert!(build_job(&run, &without_key, &config)
            .unwrap_err()
            .to_string()
            .contains("benchmark runner requires a run key secret"));
        assert!(build_job(&run, &configured, &config).is_ok());
        run.request.runner = "unconfigured".into();
        run.fingerprint = run.request.fingerprint().unwrap();
        assert!(build_job(&run, &configured, &config).is_err());
        run.request.runner = "runner".into();
        run.request.workload.name = "unapproved".into();
        run.fingerprint = run.request.fingerprint().unwrap();
        assert!(build_job(&run, &configured, &config).is_err());
        run.request.workload.name = "load".into();
        run.request.workload.max_requests = 9;
        run.fingerprint = run.request.fingerprint().unwrap();
        assert!(build_job(&run, &configured, &config).is_err());
        run.request.workload.max_requests = 100;
        run.request.workload.model_budget_tokens = 0;
        run.fingerprint = run.request.fingerprint().unwrap();
        assert!(build_job(&run, &configured, &config).is_err());
    }

    #[test]
    fn resolved_configs_are_used_per_job_without_mutating_prior_jobs() {
        for kind in [RunKind::Validation, RunKind::Benchmark] {
            let templates = templates(kind.clone(), Some("api"));
            let config = if kind == RunKind::Validation {
                validation_config()
            } else {
                benchmark_config()
            };
            let run = run(kind.clone());
            let first =
                serde_json::to_value(build_job(&run, &templates, &config).unwrap()).unwrap();
            let mut second_config = config.clone();
            second_config["base_url"] = json!("https://second.example/v1");
            let second =
                serde_json::to_value(build_job(&run, &templates, &second_config).unwrap()).unwrap();
            let name = if kind == RunKind::Validation {
                "THALAMUS_VALIDATION_CONFIG"
            } else {
                "THALAMUS_BENCHMARK_CONFIG"
            };
            let first_config = env(&first, name).unwrap()["value"].as_str().unwrap();
            let second_config_value = env(&second, name).unwrap()["value"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(first_config).unwrap(), config);
            assert_eq!(
                serde_json::from_str::<Value>(second_config_value).unwrap(),
                second_config
            );
        }
    }

    #[test]
    fn validation_job_rejects_missing_wrong_runner_and_unsafe_config() {
        let mut run = run(RunKind::Validation);
        let configured = templates(RunKind::Validation, Some("api"));
        let profile = validation_config();
        assert!(build_job(&run, &configured, &json!(null)).is_err());
        assert!(build_job(&run, &configured, &json!({})).is_err());
        assert!(build_job(&run, &templates(RunKind::Benchmark, Some("api")), &profile).is_err());
        run.request.runner = "missing".into();
        run.fingerprint = run.request.fingerprint().unwrap();
        assert!(build_job(&run, &configured, &profile).is_err());
        run.request.runner = "runner".into();
        run.fingerprint = run.request.fingerprint().unwrap();
        let mut invalid = profile.clone();
        invalid["benchmark"] = json!({});
        assert!(build_job(&run, &configured, &invalid).is_err());
        let mut invalid = profile;
        invalid["base_url"] = json!("https://user:secret@gateway.example/v1");
        assert!(build_job(&run, &configured, &invalid).is_err());
    }

    #[test]
    fn validation_capability_requires_functional_allowlist_and_key() {
        let configured = templates(RunKind::Validation, Some("api"));
        assert!(configured.supports_validation_runner("runner", "functional"));
        assert!(!configured.supports_validation_runner("missing", "functional"));
        assert!(!configured.supports_validation_runner("runner", "load"));
        assert!(!templates(RunKind::Validation, None)
            .supports_validation_runner("runner", "functional"));
        assert!(!templates(RunKind::Benchmark, Some("api"))
            .supports_validation_runner("runner", "functional"));
    }

    #[test]
    fn benchmark_supports_concurrent_shared_prefix_and_http_targets() {
        let mut value = benchmark_config();
        value["base_url"] = json!("http://gateway.example/v1");
        value["benchmark"]["workloads"]["load"]["load"] = json!({
            "type": "concurrent",
            "stages": [{"concurrency_level": 1, "num_requests": 10}]
        });
        value["benchmark"]["workloads"]["load"]["data"] = json!({
            "type": "shared_prefix",
            "num_groups": 2,
            "num_prompts_per_group": 2,
            "system_prompt_len": 16,
            "question_len": 8,
            "output_len": 8,
            "enable_multi_turn_chat": true
        });
        assert!(validate_benchmark_options(&value["benchmark"]).is_ok());
        let configured = templates(RunKind::Benchmark, Some("api"));
        assert!(build_job(&run(RunKind::Benchmark), &configured, &value).is_ok());
    }

    #[test]
    fn benchmark_runner_image_must_be_digest_pinned_and_credential_free() {
        let configured = templates(RunKind::Benchmark, Some("api"));
        for image in [
            "example/runner:latest".to_string(),
            format!(
                "user:password@registry.example/runner@sha256:{}",
                "a".repeat(64)
            ),
            format!("https://registry.example/runner@sha256:{}", "a".repeat(64)),
            format!(
                "registry.example/runner?token=secret@sha256:{}",
                "a".repeat(64)
            ),
        ] {
            let mut runners = configured.runners.clone();
            runners.get_mut("runner").unwrap().image = image;
            assert!(Templates::new(
                configured.namespace.clone(),
                runners,
                configured.result_upload.clone(),
            )
            .is_err());
        }
    }

    #[test]
    fn benchmark_config_rejects_unsafe_or_unbounded_inputs() {
        let original = benchmark_config();
        let mut cases = Vec::new();
        for url in [
            "ftp://candidate.example/v1",
            "https://user:password@candidate.example/v1",
            "https://@candidate.example/v1",
            "https://candidate.example/v1?token=secret",
            "https://candidate.example/v1#secret",
            "https://candidate.example/other",
            "https://candidate.example/v1/",
            "https://candidate.example/a b/v1",
        ] {
            let mut value = original.clone();
            value["base_url"] = json!(url);
            cases.push(value);
        }
        let mut value = original.clone();
        value["base_url"] = json!(format!("https://candidate.example/{}/v1", "x".repeat(1024)));
        cases.push(value);
        for model in ["", "  ", "model\nname", &"m".repeat(257)] {
            let mut value = original.clone();
            value["model"] = json!(model);
            cases.push(value);
        }
        let mut value = original.clone();
        value.as_object_mut().unwrap().remove("model");
        cases.push(value);
        for benchmark in [
            json!(null),
            json!([]),
            json!({}),
            json!({"api_key":"secret"}),
        ] {
            let mut value = original.clone();
            value["benchmark"] = benchmark;
            cases.push(value);
        }
        let mut value = original.clone();
        value["api_key"] = json!("secret");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["api_key"] = json!("secret");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["tokenizer"]["token"] = json!("secret");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["tokenizer"]["repository"] =
            json!("https://user:secret@example.com/repo");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["tokenizer"]["revision"] = json!("main");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["load"]["headers"] =
            json!({"Authorization":"secret"});
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["data"]["url"] = json!("https://example.com");
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["load"]["stages"][0]["rate"] = json!(257);
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["load"]["stages"][0]["duration"] = json!(0);
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["data"]["output_len"] = json!(16_385);
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["goodput"]["ttft"] = json!(0);
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["load"]["stages"] =
            json!(vec![json!({"rate":1,"duration":1}); 17]);
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"] = json!({});
        cases.push(value);
        let mut value = original.clone();
        value["benchmark"]["workloads"]["load"]["data"] = json!({
            "type":"shared_prefix",
            "num_groups":128,
            "num_prompts_per_group":128,
            "system_prompt_len":65_536,
            "question_len":65_536,
            "output_len":8,
            "enable_multi_turn_chat":true
        });
        cases.push(value);
        let run = run(RunKind::Benchmark);
        let templates = templates(RunKind::Benchmark, Some("api"));
        for value in cases {
            assert!(
                build_job(&run, &templates, &value).is_err(),
                "unexpectedly accepted {value}"
            );
        }
    }

    #[test]
    fn benchmark_options_and_config_respect_environment_size_limit() {
        let mut value = benchmark_config();
        let mut workloads = serde_json::Map::new();
        for i in 0..128 {
            let mut workload = value["benchmark"]["workloads"]["load"].clone();
            workload["load"]["stages"] = json!(vec![json!({"rate":1,"duration":1}); 16]);
            workloads.insert(format!("{i:03}{}", "x".repeat(125)), workload);
        }
        value["benchmark"]["workloads"] = Value::Object(workloads);
        assert!(validate_benchmark_options(&value["benchmark"]).is_err());
        assert!(build_job(
            &run(RunKind::Benchmark),
            &templates(RunKind::Benchmark, Some("api")),
            &value,
        )
        .is_err());
    }

    #[test]
    fn benchmark_preflight_enforces_request_time_and_token_budgets() {
        let options = benchmark_config()["benchmark"].clone();
        let workload = run(RunKind::Benchmark).request.workload;
        assert!(validate_benchmark_workload(&options, &workload).is_ok());
        let mut changed = workload.clone();
        changed.name = "unknown".into();
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed = workload.clone();
        changed.duration_seconds = 9;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed = workload.clone();
        changed.max_requests = 9;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed = workload.clone();
        changed.model_budget_tokens = 80;
        assert!(validate_benchmark_workload(&options, &changed).is_ok());
        changed.model_budget_tokens = 79;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed = workload.clone();
        changed.warmup_seconds = 1;
        changed.duration_seconds = 10;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed.duration_seconds = 11;
        changed.max_requests = 10;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
        changed.max_requests = 11;
        assert!(validate_benchmark_workload(&options, &changed).is_ok());
        changed.model_budget_tokens = 0;
        assert!(validate_benchmark_workload(&options, &changed).is_err());
    }

    #[test]
    fn benchmark_preflight_bounds_concurrency_and_random_stage_memory() {
        let mut options = benchmark_config()["benchmark"].clone();
        options["workloads"]["load"]["load"] = json!({
            "type":"concurrent",
            "stages":[{"concurrency_level":2,"num_requests":1000}]
        });
        options["workloads"]["load"]["data"] = json!({
            "type":"random", "input_len":15999, "output_len":1
        });
        let mut workload = run(RunKind::Benchmark).request.workload;
        workload.max_requests = 1000;
        workload.model_budget_tokens = 1000;
        assert!(validate_benchmark_workload(&options, &workload).is_err());
        workload.concurrency = 2;
        assert!(validate_benchmark_workload(&options, &workload).is_ok());
        options["workloads"]["load"]["data"]["input_len"] = json!(16000);
        assert!(validate_benchmark_workload(&options, &workload).is_err());
    }

    #[test]
    fn benchmark_preflight_bounds_shared_prefix_history_and_warmup_memory() {
        let mut options = benchmark_config()["benchmark"].clone();
        options["workloads"]["load"]["load"] = json!({
            "type":"concurrent",
            "stages":[{"concurrency_level":1,"num_requests":281}]
        });
        options["workloads"]["load"]["data"] = json!({
            "type":"shared_prefix",
            "num_groups":1,
            "num_prompts_per_group":1,
            "system_prompt_len":100,
            "question_len":100,
            "output_len":1,
            "enable_multi_turn_chat":true
        });
        let mut workload = run(RunKind::Benchmark).request.workload;
        workload.max_requests = 400;
        workload.model_budget_tokens = 400;
        assert!(validate_benchmark_workload(&options, &workload).is_ok());
        options["workloads"]["load"]["load"]["stages"][0]["num_requests"] = json!(283);
        assert!(validate_benchmark_workload(&options, &workload).is_err());
        options["workloads"]["load"]["load"]["stages"][0]["num_requests"] = json!(1);
        workload.duration_seconds = 300;
        workload.warmup_seconds = 300;
        assert!(validate_benchmark_workload(&options, &workload).is_err());
    }

    #[test]
    fn validation_preflight_uses_same_fields_and_environment_limit() {
        let mut config = validation_config();
        assert!(validate_observed_validation_config(&config).is_ok());
        config["api_key"] = json!("secret");
        assert!(validate_observed_validation_config(&config).is_err());
        config.as_object_mut().unwrap().remove("api_key");
        config["context_prompt"] = json!("x".repeat(64 * 1024));
        assert!(validate_observed_validation_config(&config).is_err());
    }
}
