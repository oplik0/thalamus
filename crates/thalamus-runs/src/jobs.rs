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

#[derive(Clone, Debug)]
pub struct Templates {
    namespace: String,
    runners: BTreeMap<String, JobTemplate>,
    result_upload: ResultUpload,
    validation_configs: BTreeMap<String, BTreeMap<String, String>>,
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
            validation_configs: BTreeMap::new(),
        })
    }

    /// Configurations are operator-controlled and indexed by exact service and
    /// deployment identity. They are never accepted from an API request.
    pub fn with_validation_configs(
        mut self,
        configs: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    ) -> Result<Self> {
        let mut encoded = BTreeMap::new();
        for (service, deployments) in configs {
            crate::text(&service, "service", 1024)?;
            let mut entries = BTreeMap::new();
            for (deployment, config) in deployments {
                crate::text(&deployment, "deployment", 1024)?;
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
                let base = object
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .and_then(|v| url::Url::parse(v).ok())
                    .ok_or_else(|| Error::Invalid("invalid validation base URL".into()))?;
                if !matches!(base.scheme(), "http" | "https")
                    || base.host_str().is_none()
                    || !base.username().is_empty()
                    || base.password().is_some()
                    || base.query().is_some()
                    || base.fragment().is_some()
                {
                    return Err(Error::Invalid("invalid validation base URL".into()));
                }
                let json = serde_json::to_string(&config)?;
                // Linux limits the size of each individual environment string.
                if json.len() > 64 * 1024 {
                    return Err(Error::Invalid(
                        "validation config exceeds environment limit".into(),
                    ));
                }
                entries.insert(deployment, json);
            }
            encoded.insert(service, entries);
        }
        self.validation_configs = encoded;
        Ok(self)
    }
}

/// Constructs a typed Job exclusively from a trusted runner and named workload.
/// The runner receives a presigned PUT URL only via its per-run Secret.
pub fn build_job(run: &Run, templates: &Templates) -> Result<Job> {
    run.request.validate()?;
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
    ];
    if matches!(run.request.kind, RunKind::Validation) {
        if template.key_secret.is_none() {
            return Err(Error::Invalid(
                "validation runner requires a run key secret".into(),
            ));
        }
        let config = templates
            .validation_configs
            .get(&run.request.service)
            .and_then(|deployments| deployments.get(&run.request.deployment))
            .ok_or_else(|| {
                Error::Invalid("no validation configuration for service and deployment".into())
            })?;
        env.push(json!({"name":"THALAMUS_VALIDATION_CONFIG","value":config}));
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
                    "volumes":[{"name":"result-upload","projected":{"defaultMode":292,"sources":[{"secret":{"name":secret,"items":[{"key":"url","path":"url"}]}}]}}],
                    "containers":[{"name":"runner","image":template.image,"command":template.command,"env":env,
                        "volumeMounts":[{"name":"result-upload","mountPath":"/var/run/thalamus/result-upload","readOnly":true}],
                        "resources":{"requests":{"cpu":"100m","memory":"128Mi","ephemeral-storage":"64Mi"},"limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"1Gi"}},
                        "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}}}]}}}
    });
    if matches!(template.kind, RunKind::Debug) {
        let pod = &mut value["spec"]["template"]["spec"];
        pod["volumes"].as_array_mut().unwrap().push(json!({"name":"kube-api-access","projected":{"defaultMode":420,"sources":[
            {"serviceAccountToken":{"path":"token","expirationSeconds":3600}},
            {"configMap":{"name":"kube-root-ca.crt","items":[{"key":"ca.crt","path":"ca.crt"}]}},
            {"downwardAPI":{"items":[{"path":"namespace","fieldRef":{"apiVersion":"v1","fieldPath":"metadata.namespace"}}]}}
        ]}}));
        pod["containers"][0]["volumeMounts"].as_array_mut().unwrap().push(json!({"name":"kube-api-access","mountPath":"/var/run/secrets/kubernetes.io/serviceaccount","readOnly":true}));
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
