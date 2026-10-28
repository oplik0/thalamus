//! Pure joins over actual Kubernetes JSON, shared by the live reader and fixtures.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thalamus_core::{DesiredIdentity, RuntimeIdentity};

use crate::{
    DeploymentSnapshot, Error, Result, RouteListener, RouteSnapshot, RunnerObservation,
    RuntimeConfig,
};

/// A complete point-in-time collection. Each entry is an unmodified Kubernetes
/// API object, not a projection supplied by a caller or serving workload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveResources {
    pub routes: Vec<Value>,
    pub gateways: Vec<Value>,
    pub policies: Vec<Value>,
    pub backends: Vec<Value>,
    pub pools: Vec<Value>,
    pub picker_services: Vec<Value>,
    #[serde(default)]
    pub agentic_services: Vec<Value>,
    #[serde(default)]
    pub agentic_deployments: Vec<Value>,
    /// Metadata-only reads; Secret values are never collected or persisted.
    #[serde(default)]
    pub agentic_secret_metadata: Vec<Value>,
    #[serde(default)]
    pub agentic_endpoint_slices: Vec<Value>,
    pub serving: Vec<Value>,
    pub kustomizations: Vec<Value>,
    pub sources: Vec<Value>,
    pub helm_releases: Vec<Value>,
    pub helm_charts: Vec<Value>,
    pub chart_sources: Vec<Value>,
    pub config_maps: Vec<Value>,
    pub pods: Vec<Value>,
    pub nodes: Vec<Value>,
    pub replicasets: Vec<Value>,
    pub horizontal_autoscalers: Vec<Value>,
    pub scaled_objects: Vec<Value>,
    pub workloads: Vec<Value>,
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub(crate) fn string<'a>(v: &'a Value, pointer: &str) -> Result<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("missing {pointer}")))
}
pub(crate) fn array<'a>(v: &'a Value, pointer: &str) -> Result<&'a Vec<Value>> {
    v.pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("missing array {pointer}")))
}
pub(crate) fn name(v: &Value) -> Result<&str> {
    string(v, "/metadata/name")
}
pub(crate) fn namespace(v: &Value) -> Result<&str> {
    string(v, "/metadata/namespace")
}
pub(crate) fn uid(v: &Value) -> Result<&str> {
    string(v, "/metadata/uid")
}
pub(crate) fn generation(v: &Value) -> Result<i64> {
    v.pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid("missing metadata generation"))
}
pub(crate) fn annotation<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("missing annotation {key}")))
}
fn label<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.pointer("/metadata/labels")
        .and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("missing label {key}")))
}
pub(crate) fn lookup<'a>(items: &'a [Value], ns: &str, resource: &str) -> Result<&'a Value> {
    let mut found = items.iter().filter(|v| {
        v.pointer("/metadata/name").and_then(Value::as_str) == Some(resource)
            && v.pointer("/metadata/namespace").and_then(Value::as_str) == Some(ns)
    });
    let first = found
        .next()
        .ok_or_else(|| invalid(format!("{ns}/{resource} not observed")))?;
    if found.next().is_some() {
        return Err(invalid(format!("duplicate object {ns}/{resource}")));
    }
    Ok(first)
}
struct HashWriter<'a>(&'a mut Sha256);
impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn hash_canonical(value: &Value, hash: &mut Sha256) -> Result<()> {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            hash.update(b"{");
            for (index, (key, child)) in entries.into_iter().enumerate() {
                if index != 0 {
                    hash.update(b",");
                }
                serde_json::to_writer(HashWriter(&mut *hash), key)?;
                hash.update(b":");
                hash_canonical(child, hash)?;
            }
            hash.update(b"}");
        }
        Value::Array(values) => {
            hash.update(b"[");
            for (index, child) in values.iter().enumerate() {
                if index != 0 {
                    hash.update(b",");
                }
                hash_canonical(child, hash)?;
            }
            hash.update(b"]");
        }
        _ => serde_json::to_writer(HashWriter(&mut *hash), value)?,
    }
    Ok(())
}
pub(crate) fn canonical_digest(v: &Value) -> Result<String> {
    let mut hash = Sha256::new();
    hash_canonical(v, &mut hash)?;
    Ok(format!("sha256:{:x}", hash.finalize()))
}
pub(crate) fn digest(s: &str) -> Result<String> {
    let s = s
        .rsplit_once("sha256:")
        .map(|(_, d)| d)
        .ok_or_else(|| invalid("missing sha256 image/content digest"))?;
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid lowercase sha256 digest"));
    }
    Ok(format!("sha256:{s}"))
}
fn content_digest(s: &str) -> Result<String> {
    let value = digest(s)?;
    if s != value {
        return Err(invalid("content digest is not canonical"));
    }
    Ok(value)
}
pub(crate) fn pinned_image(s: &str) -> Result<String> {
    let (repository, _) = s
        .rsplit_once("@sha256:")
        .ok_or_else(|| invalid("image is not digest-pinned"))?;
    if repository.is_empty()
        || repository.contains('@')
        || repository.bytes().any(|b| b.is_ascii_whitespace())
    {
        return Err(invalid("invalid OCI image name"));
    }
    digest(s)
}
pub(crate) fn condition(conditions: &[Value], kind: &str, object_generation: i64) -> Result<()> {
    let matching: Vec<_> = conditions
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some(kind))
        .collect();
    if matching.len() != 1
        || matching[0].get("status").and_then(Value::as_str) != Some("True")
        || matching[0]
            .get("observedGeneration")
            .and_then(Value::as_i64)
            != Some(object_generation)
        || matching[0].get("reason").and_then(Value::as_str) == Some("PartiallyValid")
    {
        return Err(invalid(format!(
            "{kind} condition is missing, stale, or not fully valid"
        )));
    }
    Ok(())
}
fn ready(v: &Value) -> Result<()> {
    condition(array(v, "/status/conditions")?, "Ready", generation(v)?)
}

pub(crate) fn parent_key(parent: &Value, route_ns: &str) -> Result<(String, String)> {
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
        return Err(invalid("unsupported route parent"));
    }
    Ok((
        parent
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or(route_ns)
            .to_owned(),
        string(parent, "/name")?.to_owned(),
    ))
}
fn hostname_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
    } else {
        pattern.eq_ignore_ascii_case(host)
    }
}
pub(crate) fn host_overlap(other: &Value, hosts: &[String]) -> bool {
    match other.pointer("/spec/hostnames").and_then(Value::as_array) {
        None => true,
        Some(v) if v.is_empty() => true,
        Some(v) => v.iter().any(|h| {
            h.as_str()
                .is_none_or(|h| hosts.iter().any(|a| hostname_matches(h, a)))
        }),
    }
}
pub(crate) fn path_overlap(other: &Value, paths: &[String]) -> bool {
    let Some(rules) = other.pointer("/spec/rules").and_then(Value::as_array) else {
        return true;
    };
    rules.iter().any(|rule| {
        let Some(matches) = rule.get("matches").and_then(Value::as_array) else {
            return true;
        };
        if matches.is_empty() {
            return true;
        }
        matches.iter().any(|m| {
            let Some(path) = m.get("path") else {
                return true;
            };
            let value = path.get("value").and_then(Value::as_str).unwrap_or("/");
            if !value.starts_with('/')
                || value.contains('%')
                || value.contains("//")
                || value.contains('?')
                || value.contains('#')
                || value.split('/').any(|p| p == "." || p == "..")
            {
                return true;
            }
            match path
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("PathPrefix")
            {
                "Exact" => paths.iter().any(|p| p == value),
                "PathPrefix" => {
                    let prefix = value.trim_end_matches('/');
                    paths.iter().any(|p| {
                        prefix.is_empty()
                            || p == prefix
                            || p.strip_prefix(prefix)
                                .is_some_and(|rest| rest.starts_with('/'))
                    })
                }
                _ => true,
            }
        })
    })
}
fn route_rules(route: &Value) -> Result<(Vec<String>, Vec<Value>, Option<String>)> {
    let rules = array(route, "/spec/rules")?;
    if rules.is_empty() {
        return Err(invalid("route has no rules"));
    }
    let mut paths = BTreeSet::new();
    let mut common: Option<Vec<Value>> = None;
    let mut common_header: Option<Option<String>> = None;
    for rule in rules {
        let matches = array(rule, "/matches")?;
        if matches.is_empty() {
            return Err(invalid("unbounded route match"));
        }
        let mut rewrite: Option<&str> = None;
        let mut header: Option<String> = None;
        if let Some(filters) = rule.get("filters") {
            for filter in filters
                .as_array()
                .ok_or_else(|| invalid("invalid route filters"))?
            {
                match filter.get("type").and_then(Value::as_str) {
                    Some("URLRewrite")
                        if rewrite.is_none()
                            && filter.pointer("/urlRewrite/hostname").is_none()
                            && filter
                                .pointer("/urlRewrite/path/type")
                                .and_then(Value::as_str)
                                == Some("ReplaceFullPath") =>
                    {
                        let target = string(filter, "/urlRewrite/path/replaceFullPath")?;
                        if target != "/v1/chat/completions" && target != "/v1/completions" {
                            return Err(invalid("unsupported rewrite target"));
                        }
                        rewrite = Some(target);
                    }
                    Some("ResponseHeaderModifier") if header.is_none() => {
                        let modifier = filter
                            .get("responseHeaderModifier")
                            .and_then(Value::as_object)
                            .ok_or_else(|| invalid("invalid response header filter"))?;
                        if modifier.len() != 1 || !modifier.contains_key("set") {
                            return Err(invalid("unsupported response header modification"));
                        }
                        let sets = array(filter, "/responseHeaderModifier/set")?;
                        if sets.len() != 1
                            || !string(&sets[0], "/name")?
                                .eq_ignore_ascii_case("x-thalamus-backend")
                        {
                            return Err(invalid("ambiguous backend header filter"));
                        }
                        header = Some(string(&sets[0], "/value")?.to_owned());
                    }
                    _ => return Err(invalid("unsupported route filter")),
                }
            }
        }
        for m in matches {
            let obj = m
                .as_object()
                .ok_or_else(|| invalid("invalid route match"))?;
            if obj.keys().any(|k| k != "path" && k != "method")
                || m.get("method")
                    .is_some_and(|method| method.as_str() != Some("POST"))
                || m.pointer("/path/type").and_then(Value::as_str) != Some("Exact")
            {
                return Err(invalid("route match is not an exact POST path"));
            }
            let path = string(m, "/path/value")?;
            if !path.starts_with('/')
                || path.contains('?')
                || path.contains('#')
                || path.contains('%')
                || path.contains("//")
                || path.split('/').any(|p| p == "." || p == "..")
            {
                return Err(invalid("unsafe route path"));
            }
            let target = rewrite.unwrap_or(path);
            if target != "/v1/chat/completions" && target != "/v1/completions" {
                return Err(invalid("unsupported inference path"));
            }
            if rewrite.is_some() && !path.ends_with(target) {
                return Err(invalid(
                    "route rewrite does not correspond to incoming path",
                ));
            }
            if !paths.insert(path.to_owned()) {
                return Err(invalid("duplicate route path"));
            }
        }
        if let Some(previous) = &common_header {
            if previous != &header {
                return Err(invalid("route paths have different backend header filters"));
            }
        } else {
            common_header = Some(header);
        }
        let refs = array(rule, "/backendRefs")?;
        if refs.is_empty() {
            return Err(invalid("route has no backends"));
        }
        let mut sorted = refs.clone();
        sorted.sort_by_key(|v| serde_json::to_string(v).unwrap_or_default());
        if let Some(previous) = &common {
            if previous != &sorted {
                return Err(invalid("route paths have different backend sets"));
            }
        } else {
            common = Some(sorted);
        }
    }
    Ok((
        paths.into_iter().collect(),
        common.ok_or_else(|| invalid("missing backend refs"))?,
        common_header.flatten(),
    ))
}

pub(crate) fn route_listeners(
    resources: &LiveResources,
    route: &Value,
    hosts: &[String],
) -> Result<Vec<RouteListener>> {
    let route_ns = namespace(route)?;
    let parents = array(route, "/spec/parentRefs")?;
    let statuses = array(route, "/status/parents")?;
    if parents.is_empty() {
        return Err(invalid("route has no parent"));
    }
    let mut listeners = BTreeSet::new();
    for parent in parents {
        let (ns, gateway_name) = parent_key(parent, route_ns)?;
        let gateway = lookup(&resources.gateways, &ns, &gateway_name)?;
        let gateway_gen = generation(gateway)?;
        condition(
            array(gateway, "/status/conditions")?,
            "Accepted",
            gateway_gen,
        )?;
        condition(
            array(gateway, "/status/conditions")?,
            "Programmed",
            gateway_gen,
        )?;
        let matching: Vec<_> = statuses
            .iter()
            .filter(|s| {
                s.get("parentRef").is_some_and(|p| {
                    parent_key(p, route_ns).ok() == Some((ns.clone(), gateway_name.clone()))
                        && p.get("sectionName").and_then(Value::as_str)
                            == parent.get("sectionName").and_then(Value::as_str)
                        && p.get("port").and_then(Value::as_u64)
                            == parent.get("port").and_then(Value::as_u64)
                })
            })
            .collect();
        if matching.len() != 1 {
            return Err(invalid("ambiguous route parent status"));
        }
        let conditions = array(matching[0], "/conditions")?;
        condition(conditions, "Accepted", generation(route)?)?;
        condition(conditions, "ResolvedRefs", generation(route)?)?;
        let spec_listeners = array(gateway, "/spec/listeners")?;
        let status_listeners = array(gateway, "/status/listeners")?;
        let mut parent_matched = false;
        for listener in spec_listeners {
            let listener_name = string(listener, "/name")?;
            if parent
                .get("sectionName")
                .and_then(Value::as_str)
                .is_some_and(|s| s != listener_name)
            {
                continue;
            }
            let port = listener
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n != 0)
                .ok_or_else(|| invalid("invalid listener port"))?;
            if parent
                .get("port")
                .and_then(Value::as_u64)
                .is_some_and(|p| p != u64::from(port))
            {
                continue;
            }
            let protocol = string(listener, "/protocol")?;
            if protocol != "HTTP" && protocol != "HTTPS" {
                continue;
            }
            let status: Vec<_> = status_listeners
                .iter()
                .filter(|s| s.get("name").and_then(Value::as_str) == Some(listener_name))
                .collect();
            if status.len() != 1 {
                return Err(invalid("missing listener status"));
            }
            condition(array(status[0], "/conditions")?, "Accepted", gateway_gen)?;
            condition(array(status[0], "/conditions")?, "Programmed", gateway_gen)?;
            for host in hosts {
                if listener
                    .get("hostname")
                    .and_then(Value::as_str)
                    .is_none_or(|pattern| hostname_matches(pattern, host))
                {
                    listeners.insert(RouteListener {
                        hostname: host.clone(),
                        port,
                        protocol: protocol.to_owned(),
                    });
                    parent_matched = true;
                }
            }
        }
        if !parent_matched {
            return Err(invalid("route has no accepted matching listener"));
        }
    }
    if hosts
        .iter()
        .any(|h| !listeners.iter().any(|l| &l.hostname == h))
    {
        return Err(invalid("hostname has no accepted listener"));
    }
    Ok(listeners.into_iter().collect())
}

pub(crate) enum RoutePolicyMode<'a> {
    Direct {
        candidate: Option<&'a str>,
    },
    AgenticOuter {
        candidate: Option<&'a str>,
    },
    AgenticInternal {
        deployment: &'a str,
        candidate: bool,
    },
    AgenticModels {
        deployment: &'a str,
        candidate: bool,
    },
}

pub(crate) fn check_route_policy(
    resources: &LiveResources,
    route: &Value,
    service: &str,
    mode: RoutePolicyMode<'_>,
    team: uuid::Uuid,
) -> Result<()> {
    let ns = namespace(route)?;
    let route_name = name(route)?;
    let (candidate, forward_bearer, internal_deployment, request_units) = match mode {
        RoutePolicyMode::Direct { candidate } => (candidate, false, None, false),
        RoutePolicyMode::AgenticOuter { candidate } => (candidate, true, None, false),
        RoutePolicyMode::AgenticInternal {
            deployment,
            candidate: true,
        } => (Some(deployment), false, None, false),
        RoutePolicyMode::AgenticInternal {
            deployment,
            candidate: false,
        } => (None, false, Some(deployment), false),
        RoutePolicyMode::AgenticModels {
            deployment,
            candidate: true,
        } => (Some(deployment), false, None, true),
        RoutePolicyMode::AgenticModels {
            deployment,
            candidate: false,
        } => (None, false, Some(deployment), true),
    };
    let policies: Vec<_> = resources
        .policies
        .iter()
        .filter(|p| {
            p.pointer("/metadata/namespace").and_then(Value::as_str) == Some(ns)
                && p.pointer("/spec/targetRefs")
                    .and_then(Value::as_array)
                    .is_some_and(|refs| {
                        refs.iter().any(|r| {
                            r.get("kind").and_then(Value::as_str) == Some("HTTPRoute")
                                && r.get("name").and_then(Value::as_str) == Some(route_name)
                                && r.get("group")
                                    .and_then(Value::as_str)
                                    .unwrap_or("gateway.networking.k8s.io")
                                    == "gateway.networking.k8s.io"
                        })
                    })
        })
        .collect();
    if policies.len() != 1 {
        return Err(invalid("route policy is missing or ambiguous"));
    }
    let policy = policies[0];
    if array(policy, "/spec/targetRefs")?.len() != 1 {
        return Err(invalid("ambiguous route policy target"));
    }
    let ancestors = array(policy, "/status/ancestors")?;
    if ancestors.len() != 1 {
        return Err(invalid("ambiguous policy ancestors"));
    }
    condition(
        array(&ancestors[0], "/conditions")?,
        "Accepted",
        generation(policy)?,
    )?;
    if policy.pointer("/spec/traffic/apiKeyAuthentication/mode").and_then(Value::as_str) != Some("Strict")
        || policy.pointer("/spec/traffic/apiKeyAuthentication/configMapSelector/matchLabels/thalamus.dev~1gateway-keys").and_then(Value::as_str) != Some("true")
        || policy.pointer("/spec/traffic/authorization/action").and_then(Value::as_str) != Some("Require") {
        return Err(invalid("route authentication or authorization is not enforced"));
    }
    let expressions = array(
        policy,
        "/spec/traffic/authorization/policy/matchExpressions",
    )?;
    let has = |s: &str| expressions.iter().any(|v| v.as_str() == Some(s));
    const UUID_RE: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
    for field in ["key_id", "user", "creator"] {
        let expression =
            format!("type(apiKey.{field}) == string && apiKey.{field}.matches(\"{UUID_RE}\")");
        if !has(&expression) {
            return Err(invalid("reviewed key identity requirement missing"));
        }
    }
    let team_prefix = format!(
        "type(apiKey.team) == string && apiKey.team.matches(\"{UUID_RE}\") && apiKey.team in "
    );
    let team_maps: Vec<_> = expressions
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| s.strip_prefix(&team_prefix))
        .collect();
    if team_maps.len() != 1 {
        return Err(invalid("reviewed team authorization missing"));
    }
    let caps: Value =
        serde_json::from_str(team_maps[0]).map_err(|_| invalid("invalid team cap map"))?;
    let team_key = team.to_string();
    if caps.as_object().is_none_or(|m| {
        m.is_empty()
            || m.iter().any(|(key, value)| {
                uuid::Uuid::parse_str(key).is_err()
                    || value
                        .as_u64()
                        .is_none_or(|n| n == 0 || n > u64::from(u32::MAX))
            })
    }) || caps.get(team_key.as_str()).is_none()
    {
        return Err(invalid("service team is not authorized by live policy"));
    }
    let project = format!(
        "(apiKey.project == null && apiKey.kind == \"personal\") || (type(apiKey.project) == string && apiKey.project.matches(\"{UUID_RE}\"))"
    );
    let service_expression = format!(
        "type(apiKey.allowed_services) == list && {} in apiKey.allowed_services",
        serde_json::to_string(service)?
    );
    let job = format!(
        "apiKey.kind != \"job\" || (type(apiKey.run_id) == string && apiKey.run_id.matches(\"{UUID_RE}\") && type(apiKey.deployment) == string && apiKey.deployment != \"\" && apiKey.run_kind in [\"validation\", \"benchmark\"] && apiKey.expiry != null && apiKey.budget != null)"
    );
    for required in [
        "type(apiKey.kind) == string && apiKey.kind in [\"personal\", \"project\", \"job\", \"debug\"]",
        project.as_str(),
        service_expression.as_str(),
        "apiKey.expiry == null || (type(apiKey.expiry) == string && request.startTime < timestamp(apiKey.expiry))",
        job.as_str(),
    ] {
        if !has(required) {
            return Err(invalid("reviewed route authorization requirement missing"));
        }
    }
    let benchmark = route_name.ends_with("-benchmark");
    let budget = if benchmark {
        "(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget >= 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget >= 0))"
    } else {
        "(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget > 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget > 0))"
    };
    if !has(budget) {
        return Err(invalid("reviewed key budget requirement missing"));
    }
    let budget_prefix = format!(
        "(apiKey.project != null || apiKey.project_budget == null) && type(apiKey.team_budget) == int && apiKey.team_budget == {}[apiKey.team] && apiKey.budget_window == ",
        team_maps[0]
    );
    let windows: Vec<_> = expressions
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| s.strip_prefix(&budget_prefix))
        .filter_map(|s| s.strip_suffix("[apiKey.team]"))
        .collect();
    if windows.len() != 1 {
        return Err(invalid("reviewed team budget requirement missing"));
    }
    let window_map: Value =
        serde_json::from_str(windows[0]).map_err(|_| invalid("invalid team window map"))?;
    if window_map.as_object().is_none_or(|m| {
        m.len() != caps.as_object().map_or(0, |c| c.len())
            || m.iter().any(|(key, value)| {
                caps.get(key.as_str()).is_none() || !matches!(value.as_str(), Some("day" | "month"))
            })
    }) {
        return Err(invalid("team budget windows are incomplete"));
    }
    if let Some(deployment) = candidate {
        let scope = format!(
            "apiKey.kind == \"job\" && apiKey.deployment == {}",
            serde_json::to_string(deployment)?
        );
        if !has(&scope) {
            return Err(invalid("candidate policy is not bound to its deployment"));
        }
    }
    if let Some(deployment) = internal_deployment {
        let scope = format!(
            "apiKey.kind != \"job\" || apiKey.deployment == {}",
            serde_json::to_string(deployment)?
        );
        if !has(&scope) {
            return Err(invalid(
                "internal job policy is not bound to its deployment",
            ));
        }
    }
    let run_scope = if benchmark {
        "apiKey.kind == \"job\" && apiKey.run_kind == \"benchmark\" && type(apiKey.request_budget) == int && apiKey.request_budget > 0 && apiKey.request_budget <= 1000000"
    } else {
        "apiKey.kind != \"job\" || apiKey.run_kind == \"validation\""
    };
    if !has(run_scope) {
        return Err(invalid(
            "route policy does not enforce the reviewed run kind",
        ));
    }
    let removes = array(policy, "/spec/traffic/transformation/request/remove")?;
    if !removes.iter().any(|v| {
        v.as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("x-thalamus-backend"))
    }) {
        return Err(invalid("client backend header is not removed"));
    }
    let sets = array(policy, "/spec/traffic/transformation/request/set")?;
    if forward_bearer {
        if sets.len() != 1
            || !sets[0]
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case("authorization"))
            || sets[0].get("value").and_then(Value::as_str)
                != Some("\"Bearer \" + apiKey.key.unredacted()")
            || policy.pointer("/spec/traffic/rateLimit").is_some()
        {
            return Err(invalid(
                "agentic route does not exclusively forward the verified bearer",
            ));
        }
        for required in [
            "x-api-key",
            "x-thalamus-user",
            "x-thalamus-project",
            "x-thalamus-team",
        ] {
            if !removes
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(required)))
            {
                return Err(invalid(
                    "agentic route does not remove untrusted credentials or attribution",
                ));
            }
        }
    } else {
        for (name, expression) in [
            ("x-thalamus-user", "apiKey.user"),
            ("x-thalamus-project", "apiKey.project"),
            ("x-thalamus-team", "apiKey.team"),
        ] {
            let matches: Vec<_> = sets
                .iter()
                .filter(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case(name))
                })
                .collect();
            if matches.len() != 1
                || matches[0].get("value").and_then(Value::as_str) != Some(expression)
            {
                return Err(invalid("trusted attribution header is missing"));
            }
        }
    }
    if policy
        .pointer("/spec/traffic/transformation/request/metadata/thalamus_service")
        .and_then(Value::as_str)
        != Some(serde_json::to_string(service)?.as_str())
    {
        return Err(invalid("trusted service attribution is missing"));
    }
    if policy
        .pointer("/spec/traffic/transformation/response")
        .is_some()
        || (!forward_bearer
            && sets.iter().any(|v| {
                v.get("name").and_then(Value::as_str).is_some_and(|s| {
                    s.eq_ignore_ascii_case("authorization")
                        || s.eq_ignore_ascii_case("x-thalamus-backend")
                })
            }))
    {
        return Err(invalid("unsafe route transformation"));
    }
    if !forward_bearer {
        let limiter = policy
            .pointer("/spec/traffic/rateLimit/global")
            .ok_or_else(|| invalid("global rate limit missing"))?;
        if limiter.get("failureMode").and_then(Value::as_str) != Some("FailClosed") {
            return Err(invalid("rate limiter is not fail-closed"));
        }
        let descriptors = array(limiter, "/descriptors")?;
        let team_cap = format!("{}[apiKey.team]", team_maps[0]);
        let unit = format!("{}[apiKey.team]", windows[0]);
        let expected: Vec<(&str, &str, &str, String)> = if benchmark {
            vec![(
                "benchmark_key",
                "apiKey.key_id",
                "Requests",
                format!("{{\"unit\":{unit},\"requestsPerUnit\":apiKey.request_budget}}"),
            )]
        } else {
            let key_limit = format!(
                "(apiKey.budget == null || apiKey.budget > {team_cap} ? {team_cap} : apiKey.budget)"
            );
            let project_limit = format!(
                "(apiKey.project_budget == null || apiKey.project_budget > {team_cap} ? {team_cap} : apiKey.project_budget)"
            );
            let counter_unit = if request_units { "Requests" } else { "Tokens" };
            vec![
                (
                    "key_id",
                    "apiKey.key_id",
                    counter_unit,
                    format!("{{\"unit\":{unit},\"requestsPerUnit\":{key_limit}}}"),
                ),
                (
                    "project",
                    "apiKey.project",
                    counter_unit,
                    format!("{{\"unit\":{unit},\"requestsPerUnit\":{project_limit}}}"),
                ),
                (
                    "team",
                    "apiKey.team",
                    counter_unit,
                    format!("{{\"unit\":{unit},\"requestsPerUnit\":{team_cap}}}"),
                ),
            ]
        };
        if descriptors.len() != expected.len() {
            return Err(invalid("route rate-limit descriptors are incomplete"));
        }
        for (descriptor, (name, expression, unit, limit)) in descriptors.iter().zip(expected.iter())
        {
            let entries = array(descriptor, "/entries")?;
            if entries.len() != 1
                || entries[0].get("name").and_then(Value::as_str) != Some(*name)
                || entries[0].get("expression").and_then(Value::as_str) != Some(*expression)
                || descriptor.get("unit").and_then(Value::as_str) != Some(*unit)
                || descriptor.get("limitOverride").and_then(Value::as_str) != Some(limit.as_str())
            {
                return Err(invalid(
                    "route rate-limit descriptor is not the reviewed policy",
                ));
            }
        }
    }
    for extra in &resources.policies {
        if std::ptr::eq(extra, policy) {
            continue;
        }
        let refs = extra.pointer("/spec/targetRefs").and_then(Value::as_array);
        if refs.is_some_and(|refs| {
            refs.iter().any(|r| {
                matches!(
                    r.get("kind").and_then(Value::as_str),
                    Some("AgentgatewayBackend" | "InferencePool")
                )
            })
        }) {
            return Err(invalid("additional backend policy can alter routing"));
        }
        if refs.is_some_and(|refs| {
            refs.iter()
                .any(|r| r.get("kind").and_then(Value::as_str) == Some("Gateway"))
        }) && extra.pointer("/spec/traffic").is_some()
        {
            return Err(invalid(
                "gateway-level traffic policy can alter route semantics",
            ));
        }
    }
    Ok(())
}

/// Pure, fail-closed semantic join. Callers cannot supply identities through config.
pub fn join_route(
    config: &RuntimeConfig,
    service: &str,
    route_name: &str,
    r: &LiveResources,
    observed_at: DateTime<Utc>,
) -> Result<RouteSnapshot> {
    let service_routes = config
        .routes
        .get(service)
        .ok_or_else(|| invalid("unknown service"))?;
    if service_routes.service.name != service
        || service_routes.team_id.is_nil()
        || service_routes.project_id.is_nil()
    {
        return Err(invalid("invalid service configuration"));
    }
    let candidate_deployment = if service_routes.public == route_name {
        None
    } else {
        Some(
            route_name
                .strip_suffix("-candidate")
                .or_else(|| route_name.strip_suffix("-benchmark"))
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 63
                        && !name.starts_with('-')
                        && !name.ends_with('-')
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
                .ok_or_else(|| invalid("route is not a derived deployment route"))?,
        )
    };
    let candidate = candidate_deployment.is_some();
    let route = lookup(&r.routes, &config.namespace, route_name)?;
    if route.pointer("/metadata/deletionTimestamp").is_some() {
        return Err(invalid("route is terminating"));
    }
    let mut hosts = BTreeSet::new();
    for h in array(route, "/spec/hostnames")? {
        let h = h
            .as_str()
            .filter(|h| !h.is_empty() && !h.contains('*') && !h.contains(':') && !h.ends_with('.'))
            .ok_or_else(|| invalid("route hostname is not exact"))?;
        if !hosts.insert(h.to_ascii_lowercase()) {
            return Err(invalid("duplicate hostname"));
        }
    }
    if hosts.is_empty() {
        return Err(invalid("route requires explicit hostnames"));
    }
    let hostnames: Vec<String> = hosts.into_iter().collect();
    let (paths, refs, response_header) = route_rules(route)?;
    if candidate && refs.len() != 1 {
        return Err(invalid("candidate route must have exactly one backend"));
    }
    let listeners = route_listeners(r, route, &hostnames)?;
    let ports = listeners
        .iter()
        .map(|l| l.port)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let target_parents: BTreeSet<_> = array(route, "/spec/parentRefs")?
        .iter()
        .map(|p| parent_key(p, &config.namespace))
        .collect::<Result<_>>()?;
    for other in &r.routes {
        if namespace(other)? == config.namespace && name(other)? == route_name {
            continue;
        }
        let other_ns = namespace(other)?;
        let Some(parents) = other.pointer("/spec/parentRefs").and_then(Value::as_array) else {
            continue;
        };
        let same_parent = parents
            .iter()
            .any(|p| parent_key(p, other_ns).is_ok_and(|k| target_parents.contains(&k)));
        if same_parent && host_overlap(other, &hostnames) && path_overlap(other, &paths) {
            return Err(invalid(format!(
                "competing route {other_ns}/{}",
                name(other)?
            )));
        }
    }
    check_route_policy(
        r,
        route,
        service,
        RoutePolicyMode::Direct {
            candidate: candidate_deployment,
        },
        service_routes.team_id,
    )?;
    let mut deployments = Vec::new();
    let mut weights = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for backend_ref in &refs {
        let group = backend_ref.get("group").and_then(Value::as_str);
        let kind = backend_ref.get("kind").and_then(Value::as_str);
        let direct = group == Some("inference.networking.k8s.io") && kind == Some("InferencePool");
        if !(direct || group == Some("agentgateway.dev") && kind == Some("AgentgatewayBackend"))
            || backend_ref
                .get("namespace")
                .and_then(Value::as_str)
                .is_some_and(|n| n != config.namespace)
            || backend_ref.get("filters").is_some()
            || (!direct && backend_ref.get("port").is_some())
            || backend_ref
                .get("weight")
                .is_some_and(|v| v.as_i64().is_none_or(|n| n < 0))
        {
            return Err(invalid("unsupported route backend reference"));
        }
        let backend_name = string(backend_ref, "/name")?;
        if !seen.insert(backend_name) {
            return Err(invalid("duplicate backend reference"));
        }
        let target = if direct {
            if refs.len() != 1 || !candidate || !route_name.ends_with("-benchmark") {
                return Err(invalid(
                    "direct pool route is not an isolated benchmark route",
                ));
            }
            let header = response_header
                .as_deref()
                .ok_or_else(|| invalid("direct pool route has no response identity"))?;
            let parts: Vec<_> = header.split('/').collect();
            if parts.len() != 3
                || parts[0] != service
                || route_name != format!("{}-benchmark", parts[1])
            {
                return Err(invalid("direct pool header does not bind route"));
            }
            let actual_name = format!("{}-backend", parts[1]);
            let actual_backend = lookup(&r.backends, &config.namespace, &actual_name)?;
            if actual_backend
                .pointer("/spec/ai/provider/custom/backendRef/name")
                .and_then(Value::as_str)
                != Some(backend_name)
            {
                return Err(invalid(
                    "direct benchmark pool differs from chat backend pool",
                ));
            }
            if let Some(port) = backend_ref.get("port") {
                let pool = lookup(&r.pools, &config.namespace, backend_name)?;
                if port.as_u64() != Some(pool_port(pool)?) {
                    return Err(invalid("direct pool target port mismatch"));
                }
            }
            let target = join_backend(config, service, &actual_name, route, r)?;
            if target.backend != header || target.revision != parts[2] {
                return Err(invalid("benchmark header differs from live revision"));
            }
            target
        } else {
            if response_header.is_some() {
                return Err(invalid("route overrides backend identity"));
            }
            join_backend(config, service, backend_name, route, r)?
        };
        let weight = backend_ref
            .get("weight")
            .map_or(Some(1), Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("invalid backend weight"))?;
        if weights.insert(target.deployment.clone(), weight).is_some() {
            return Err(invalid("duplicate deployment"));
        }
        deployments.push(target);
    }
    deployments.sort_by(|a, b| {
        (&a.deployment, &a.revision, &a.backend).cmp(&(&b.deployment, &b.revision, &b.backend))
    });
    if deployments
        .windows(2)
        .any(|w| w[0].deployment == w[1].deployment || w[0].backend == w[1].backend)
    {
        return Err(invalid("ambiguous deployment or backend identity"));
    }
    if candidate_deployment
        .is_some_and(|expected| deployments.len() != 1 || deployments[0].deployment != expected)
    {
        return Err(invalid("derived route does not target its deployment"));
    }
    Ok(RouteSnapshot {
        observed_at,
        service: service.to_owned(),
        route: route_name.to_owned(),
        agentic: false,
        hostnames,
        paths,
        ports,
        listeners,
        weights,
        deployments,
    })
}

pub(crate) fn flux_revision(
    r: &LiveResources,
    resource: &Value,
    backend: &Value,
    pool: &Value,
    raw: bool,
) -> Result<(String, Value)> {
    let helm = match (
        label(resource, "helm.toolkit.fluxcd.io/name"),
        label(resource, "helm.toolkit.fluxcd.io/namespace"),
    ) {
        (Ok(name), Ok(ns)) => {
            for object in [backend, resource] {
                if label(object, "helm.toolkit.fluxcd.io/name")? != name
                    || label(object, "helm.toolkit.fluxcd.io/namespace")? != ns
                {
                    return Err(invalid("resources do not belong to the same HelmRelease"));
                }
            }
            if raw
                && (label(pool, "helm.toolkit.fluxcd.io/name")? != name
                    || label(pool, "helm.toolkit.fluxcd.io/namespace")? != ns)
            {
                return Err(invalid("pool does not belong to the same HelmRelease"));
            }
            let release = lookup(&r.helm_releases, ns, name)?;
            ready(release)?;
            Some(release)
        }
        (Err(_), Err(_)) => None,
        _ => return Err(invalid("incomplete HelmRelease owner labels")),
    };
    let owner = helm.unwrap_or(resource);
    let flux_name = label(owner, "kustomize.toolkit.fluxcd.io/name")?;
    let flux_ns = label(owner, "kustomize.toolkit.fluxcd.io/namespace")?;
    if helm.is_none() {
        for object in [backend, resource] {
            if label(object, "kustomize.toolkit.fluxcd.io/name")? != flux_name
                || label(object, "kustomize.toolkit.fluxcd.io/namespace")? != flux_ns
            {
                return Err(invalid(
                    "resources are not owned by the same Flux Kustomization",
                ));
            }
        }
    }
    let flux = lookup(&r.kustomizations, flux_ns, flux_name)?;
    ready(flux)?;
    let applied = string(flux, "/status/lastAppliedRevision")?;
    if string(flux, "/status/lastAttemptedRevision")? != applied {
        return Err(invalid("Flux last attempted and applied revisions differ"));
    }
    let (reference, hash) = applied
        .rsplit_once('@')
        .ok_or_else(|| invalid("invalid Flux Git revision"))?;
    let valid_hash = hash.strip_prefix("sha1:").is_some_and(|s| {
        s.len() == 40
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) || hash.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if reference.is_empty() || !valid_hash {
        return Err(invalid("invalid Flux Git revision"));
    }
    if string(flux, "/spec/sourceRef/kind")? != "GitRepository" {
        return Err(invalid("Flux source is not a GitRepository"));
    }
    let source_ns = flux
        .pointer("/spec/sourceRef/namespace")
        .and_then(Value::as_str)
        .unwrap_or(flux_ns);
    let source = lookup(&r.sources, source_ns, string(flux, "/spec/sourceRef/name")?)?;
    ready(source)?;
    if string(source, "/status/artifact/revision")? != applied {
        return Err(invalid("Flux source and applied revisions differ"));
    }
    let entries = array(flux, "/status/inventory/entries")?;
    let contains = |v: &Value, group: &str, kind: &str| -> Result<bool> {
        let id = format!("{}_{}_{}_{}", namespace(v)?, name(v)?, group, kind);
        Ok(entries
            .iter()
            .any(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str())))
    };
    let chart_dependency = if let Some(release) = helm {
        if !contains(release, "helm.toolkit.fluxcd.io", "HelmRelease")? {
            return Err(invalid(
                "Flux inventory does not contain the owning HelmRelease",
            ));
        }
        if let Some(chart) = release.pointer("/spec/chart/spec") {
            let version = string(chart, "/version")?;
            if version.is_empty()
                || version.len() > 128
                || !version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
            {
                return Err(invalid("Helm chart version is not pinned"));
            }
            let chart_ref = string(release, "/status/helmChart")?;
            let (chart_ns, chart_name) = chart_ref
                .split_once('/')
                .ok_or_else(|| invalid("invalid HelmChart reference"))?;
            let actual = lookup(&r.helm_charts, chart_ns, chart_name)?;
            ready(actual)?;
            if actual.pointer("/spec/version").and_then(Value::as_str) != Some(version)
                || string(chart, "/chart")? != string(actual, "/spec/chart")?
            {
                return Err(invalid(
                    "HelmChart spec differs from pinned HelmRelease chart",
                ));
            }
            let artifact_revision = string(actual, "/status/artifact/revision")?;
            if string(release, "/status/lastAttemptedRevision")? != artifact_revision {
                return Err(invalid("HelmRelease and HelmChart revisions differ"));
            }
            let artifact_digest = content_digest(string(actual, "/status/artifact/digest")?)?;
            let source_kind = string(actual, "/spec/sourceRef/kind")?;
            let source_name = string(actual, "/spec/sourceRef/name")?;
            if chart.pointer("/sourceRef/kind").and_then(Value::as_str) != Some(source_kind)
                || chart.pointer("/sourceRef/name").and_then(Value::as_str) != Some(source_name)
            {
                return Err(invalid("HelmChart source differs from HelmRelease"));
            }
            let source_ns = actual
                .pointer("/spec/sourceRef/namespace")
                .and_then(Value::as_str)
                .unwrap_or(chart_ns);
            let declared_ns = chart
                .pointer("/sourceRef/namespace")
                .and_then(Value::as_str)
                .unwrap_or(namespace(release)?);
            if source_ns != declared_ns {
                return Err(invalid(
                    "HelmChart source namespace differs from HelmRelease",
                ));
            }
            let matching: Vec<_> = r
                .chart_sources
                .iter()
                .filter(|s| {
                    namespace(s).ok() == Some(source_ns)
                        && name(s).ok() == Some(source_name)
                        && s.get("kind").and_then(Value::as_str) == Some(source_kind)
                })
                .collect();
            if matching.len() != 1 {
                return Err(invalid("HelmChart source is missing or ambiguous"));
            }
            ready(matching[0])?;
            json!({"release":release.get("spec"), "chart": actual.get("spec"), "revision":artifact_revision,
                "digest":artifact_digest, "source": matching[0].get("spec"), "source_artifact": matching[0].pointer("/status/artifact")})
        } else if let Some(reference) = release.pointer("/spec/chartRef") {
            if string(reference, "/kind")? != "OCIRepository" {
                return Err(invalid("unsupported Helm chart reference"));
            }
            let source_ns = reference
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or(namespace(release)?);
            let matching: Vec<_> = r
                .chart_sources
                .iter()
                .filter(|s| {
                    namespace(s).ok() == Some(source_ns)
                        && name(s).ok() == reference.get("name").and_then(Value::as_str)
                        && s.get("kind").and_then(Value::as_str) == Some("OCIRepository")
                })
                .collect();
            if matching.len() != 1 {
                return Err(invalid("Helm OCI source is missing or ambiguous"));
            }
            let source = matching[0];
            ready(source)?;
            let pinned = content_digest(string(source, "/spec/ref/digest")?)?;
            let revision = string(source, "/status/artifact/revision")?;
            if !revision.ends_with(&pinned)
                || !string(release, "/status/lastAttemptedRevision")?.contains(&pinned)
            {
                return Err(invalid(
                    "HelmRelease OCI revision differs from pinned manifest",
                ));
            }
            let artifact = content_digest(string(source, "/status/artifact/digest")?)?;
            json!({"release":release.get("spec"),"source":source.get("spec"),"revision":revision,"digest":artifact})
        } else {
            return Err(invalid("HelmRelease has no pinned chart"));
        }
    } else {
        if !contains(
            resource,
            if raw { "apps" } else { "serving.kserve.io" },
            if raw {
                "Deployment"
            } else {
                "LLMInferenceService"
            },
        )? || !contains(backend, "agentgateway.dev", "AgentgatewayBackend")?
            || (raw && !contains(pool, "inference.networking.k8s.io", "InferencePool")?)
        {
            return Err(invalid(
                "Flux inventory does not bind the serving resources",
            ));
        }
        Value::Null
    };
    Ok((applied.to_owned(), chart_dependency))
}

pub(crate) fn selector(pool: &Value) -> Result<BTreeMap<String, String>> {
    let selector = pool
        .pointer("/spec/selector")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("pool selector missing"))?;
    let labels = if let Some(match_labels) = selector.get("matchLabels") {
        if selector.keys().any(|key| key != "matchLabels") {
            return Err(invalid("unsupported pool selector expression"));
        }
        match_labels
            .as_object()
            .ok_or_else(|| invalid("invalid pool labels"))?
    } else {
        selector
    };
    if labels.is_empty() {
        return Err(invalid("empty pool selector"));
    }
    labels
        .iter()
        .map(|(k, v)| {
            let v = v
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("invalid pool selector label"))?;
            Ok((k.clone(), v.to_owned()))
        })
        .collect()
}

pub(crate) fn picker_name(pool: &Value) -> Result<&str> {
    let reference = pool
        .pointer("/spec/endpointPickerRef")
        .or_else(|| pool.pointer("/spec/extensionRef"))
        .ok_or_else(|| invalid("InferencePool endpoint picker reference missing"))?;
    if reference
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| k != "Service")
        || reference
            .get("group")
            .and_then(Value::as_str)
            .is_some_and(|g| !g.is_empty())
        || reference
            .get("namespace")
            .and_then(Value::as_str)
            .is_some_and(|ns| namespace(pool).ok() != Some(ns))
    {
        return Err(invalid("unsupported endpoint picker reference"));
    }
    string(reference, "/name")
}

pub(crate) fn service_selector(service: &Value) -> Result<BTreeMap<String, String>> {
    let labels = service
        .pointer("/spec/selector")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("picker Service selector missing"))?;
    if labels.is_empty() {
        return Err(invalid("empty picker Service selector"));
    }
    labels
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("invalid picker selector"))?
                    .to_owned(),
            ))
        })
        .collect()
}

pub(crate) fn selected(pod: &Value, labels: &BTreeMap<String, String>) -> bool {
    let Some(actual) = pod.pointer("/metadata/labels").and_then(Value::as_object) else {
        return false;
    };
    labels
        .iter()
        .all(|(key, value)| actual.get(key).and_then(Value::as_str) == Some(value))
}

fn pool_port(pool: &Value) -> Result<u64> {
    let port = if let Some(ports) = pool.pointer("/spec/targetPorts").and_then(Value::as_array) {
        if ports.len() != 1 {
            return Err(invalid("ambiguous pool target ports"));
        }
        ports[0].get("number").and_then(Value::as_u64)
    } else {
        pool.pointer("/spec/targetPortNumber")
            .and_then(Value::as_u64)
    };
    port.filter(|port| *port > 0 && *port <= 65535)
        .ok_or_else(|| invalid("invalid pool target port"))
}

fn check_node(r: &LiveResources, pod: &Value) -> Result<()> {
    let node_name = string(pod, "/spec/nodeName")?;
    let matches: Vec<_> = r
        .nodes
        .iter()
        .filter(|node| node.pointer("/metadata/name").and_then(Value::as_str) == Some(node_name))
        .collect();
    if matches.len() != 1 || !pod_condition(matches[0], "Ready") {
        return Err(invalid("serving node is missing or not Ready"));
    }
    uid(matches[0])?;
    if pod.pointer("/spec/affinity/nodeAffinity").is_some() {
        return Err(invalid("unsupported node affinity"));
    }
    let selector = pod
        .pointer("/spec/nodeSelector")
        .and_then(Value::as_object)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("serving pod has no hardware node selector"))?;
    let labels = matches[0]
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("node labels missing"))?;
    if selector
        .iter()
        .any(|(key, value)| labels.get(key) != Some(value))
    {
        return Err(invalid("serving node does not match hardware selector"));
    }
    let main = unique_named(array(pod, "/spec/containers")?, "main")?;
    let quantity = |value: &Value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
    };
    let requested = main
        .pointer("/resources/limits/nvidia.com~1gpu")
        .and_then(quantity)
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid("serving GPU limit missing"))?;
    let available = matches[0]
        .pointer("/status/allocatable/nvidia.com~1gpu")
        .and_then(quantity)
        .ok_or_else(|| invalid("node GPU allocatable missing"))?;
    if available < requested {
        return Err(invalid("node cannot satisfy serving GPU limit"));
    }
    Ok(())
}

pub(crate) fn pod_condition(pod: &Value, ty: &str) -> bool {
    pod.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            let matching: Vec<_> = conditions
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some(ty))
                .collect();
            matching.len() == 1 && matching[0].get("status").and_then(Value::as_str) == Some("True")
        })
}
pub(crate) fn controller_owner(v: &Value) -> Result<&Value> {
    let refs = array(v, "/metadata/ownerReferences")?;
    let owners: Vec<_> = refs
        .iter()
        .filter(|r| r.get("controller").and_then(Value::as_bool) == Some(true))
        .collect();
    if owners.len() != 1 {
        return Err(invalid("missing or ambiguous controller owner"));
    }
    Ok(owners[0])
}
pub(crate) fn rollout(deployment: &Value, raw: bool) -> Result<()> {
    if raw
        && !matches!(
            deployment
                .pointer("/spec/strategy/type")
                .and_then(Value::as_str),
            Some("Recreate" | "RollingUpdate")
        )
    {
        return Err(invalid("unsupported raw deployment rollout strategy"));
    }
    let desired = deployment
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    if desired < 1
        || deployment
            .pointer("/status/observedGeneration")
            .and_then(Value::as_i64)
            != Some(generation(deployment)?)
        || deployment
            .pointer("/status/replicas")
            .and_then(Value::as_i64)
            != Some(desired)
        || deployment
            .pointer("/status/updatedReplicas")
            .and_then(Value::as_i64)
            != Some(desired)
        || deployment
            .pointer("/status/readyReplicas")
            .and_then(Value::as_i64)
            != Some(desired)
        || deployment
            .pointer("/status/availableReplicas")
            .and_then(Value::as_i64)
            != Some(desired)
        || deployment
            .pointer("/status/unavailableReplicas")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            != 0
    {
        return Err(invalid("serving Deployment rollout is not complete"));
    }
    Ok(())
}

/// Walk actual controller UIDs; names/labels alone do not prove ownership.
pub(crate) fn owner_chain<'a>(
    r: &'a LiveResources,
    pod: &'a Value,
    target: &'a Value,
    raw: bool,
) -> Result<Option<&'a Value>> {
    let ns = namespace(pod)?;
    let target_uid = uid(target)?;
    let mut current = pod;
    let mut deployment = None;
    let mut visited = BTreeSet::new();
    for _ in 0..6 {
        let owner = controller_owner(current)?;
        let owner_uid = string(owner, "/uid")?;
        if !visited.insert(owner_uid.to_owned()) {
            return Err(invalid("owner reference cycle"));
        }
        if owner_uid == target_uid {
            if string(owner, "/name")? != name(target)?
                || string(owner, "/kind")? != string(target, "/kind")?
            {
                return Err(invalid("owner reference identity mismatch"));
            }
            if raw && deployment.is_none() {
                rollout(target, true)?;
                deployment = Some(target);
            }
            return Ok(deployment);
        }
        let kind = string(owner, "/kind")?;
        if kind != "ReplicaSet" && kind != "Deployment" && kind != "StatefulSet" {
            return Err(invalid("unsupported workload owner chain"));
        }
        let next = lookup(&r.workloads, ns, string(owner, "/name")?)?;
        if uid(next)? != owner_uid || string(next, "/kind")? != kind {
            return Err(invalid("workload owner UID mismatch"));
        }
        if kind == "Deployment" {
            if deployment.is_some() {
                return Err(invalid("multiple deployments in owner chain"));
            }
            rollout(next, false)?;
            deployment = Some(next);
        }
        current = next;
    }
    Err(invalid("owner chain does not reach serving resource"))
}

fn unique_named<'a>(values: &'a [Value], wanted: &str) -> Result<&'a Value> {
    let mut matches = values
        .iter()
        .filter(|v| v.get("name").and_then(Value::as_str) == Some(wanted));
    let value = matches
        .next()
        .ok_or_else(|| invalid(format!("missing container {wanted}")))?;
    if matches.next().is_some() {
        return Err(invalid(format!("duplicate container {wanted}")));
    }
    Ok(value)
}
pub(crate) fn image_ids(pod: &Value, require_ready: bool) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for (spec_path, status_path, init) in [
        ("/spec/containers", "/status/containerStatuses", false),
        (
            "/spec/initContainers",
            "/status/initContainerStatuses",
            true,
        ),
    ] {
        let spec = pod.pointer(spec_path).and_then(Value::as_array);
        if spec.is_none_or(Vec::is_empty) {
            if !init {
                return Err(invalid("pod has no containers"));
            }
            if pod
                .pointer(status_path)
                .and_then(Value::as_array)
                .is_some_and(|statuses| !statuses.is_empty())
            {
                return Err(invalid("unexpected init container statuses"));
            }
            continue;
        }
        let spec = spec.unwrap();
        let statuses = array(pod, status_path)?;
        if statuses.len() != spec.len() {
            return Err(invalid("container statuses incomplete"));
        }
        for container in spec {
            let container_name = string(container, "/name")?;
            let status = unique_named(statuses, container_name)?;
            if init {
                if status
                    .pointer("/state/terminated/exitCode")
                    .and_then(Value::as_i64)
                    != Some(0)
                {
                    return Err(invalid("init container did not complete successfully"));
                }
            } else if require_ready && status.get("ready").and_then(Value::as_bool) != Some(true) {
                return Err(invalid("container is not ready"));
            }
            let key = format!(
                "{}/{}",
                if init { "init" } else { "container" },
                container_name
            );
            if images
                .insert(key, digest(string(status, "/imageID")?)?)
                .is_some()
            {
                return Err(invalid("duplicate container"));
            }
        }
    }
    if pod
        .pointer("/spec/ephemeralContainers")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid("ephemeral containers change runtime identity"));
    }
    Ok(images)
}
fn safe_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains("//") && !path.split('/').any(|s| s == "." || s == "..")
}
fn paths_overlap(a: &str, b: &str) -> bool {
    fn beneath(child: &str, parent: &str) -> bool {
        parent == "/"
            || child == parent
            || child
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/'))
    }
    beneath(a, b) || beneath(b, a)
}
fn engine_configuration(pod: &Value, expected_image: &str, model_path: &str) -> Result<Value> {
    let containers = array(pod, "/spec/containers")?;
    let engine = unique_named(containers, "main")?;
    if engine.as_object().is_none_or(|o| {
        o.keys().any(|key| {
            ![
                "name",
                "image",
                "command",
                "args",
                "env",
                "resources",
                "securityContext",
                "volumeMounts",
                "ports",
                "readinessProbe",
                "livenessProbe",
                "startupProbe",
                "imagePullPolicy",
                "terminationMessagePath",
                "terminationMessagePolicy",
            ]
            .contains(&key.as_str())
        })
    }) {
        return Err(invalid("unreviewed engine container field"));
    }
    if engine
        .pointer("/readinessProbe/httpGet/path")
        .and_then(Value::as_str)
        .is_none_or(|path| !path.starts_with('/'))
        || engine.pointer("/readinessProbe/httpGet/port").is_none()
    {
        return Err(invalid("engine has no HTTP readiness probe"));
    }
    if string(engine, "/image")? != expected_image {
        return Err(invalid("pod engine image differs from curated image"));
    }
    if engine
        .pointer("/securityContext/readOnlyRootFilesystem")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(invalid("model root filesystem is writable"));
    }
    let command = array(engine, "/command")?;
    if command.len() != 3
        || command[0].as_str() != Some("vllm")
        || command[1].as_str() != Some("serve")
        || command[2].as_str() != Some(model_path)
    {
        return Err(invalid("engine does not serve curated model path"));
    }
    let args = array(engine, "/args")?;
    let tokenizer_positions: Vec<_> = args
        .iter()
        .enumerate()
        .filter(|(_, v)| v.as_str() == Some("--tokenizer"))
        .map(|(i, _)| i)
        .collect();
    if tokenizer_positions.len() != 1
        || args.get(tokenizer_positions[0] + 1).and_then(Value::as_str) != Some(model_path)
        || args.iter().any(|v| {
            v.as_str().is_some_and(|s| {
                s.starts_with("--tokenizer=") || s == "--model" || s.starts_with("--model=")
            })
        })
    {
        return Err(invalid("tokenizer or model path is ambiguous"));
    }
    for container in containers.iter().chain(
        pod.pointer("/spec/initContainers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
    ) {
        if container
            .get("envFrom")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
            || container
                .get("env")
                .and_then(Value::as_array)
                .is_some_and(|env| env.iter().any(|e| e.get("valueFrom").is_some()))
        {
            return Err(invalid("unresolved runtime environment dependency"));
        }
    }
    let volumes = pod.pointer("/spec/volumes").and_then(Value::as_array);
    for container in containers {
        for mount in container
            .get("volumeMounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = string(mount, "/mountPath")?;
            if !safe_path(path)
                || (container.get("name").and_then(Value::as_str) == Some("main")
                    && paths_overlap(path, model_path))
            {
                return Err(invalid("volume overlays model path"));
            }
            let volume_name = string(mount, "/name")?;
            let volume = volumes
                .and_then(|v| {
                    v.iter()
                        .find(|v| v.get("name").and_then(Value::as_str) == Some(volume_name))
                })
                .ok_or_else(|| invalid("container references missing volume"))?;
            // Service-account credentials are not engine configuration. All other
            // external configuration mounts must be independently resolved; this
            // observer deliberately fails closed instead of hashing a mutable name.
            let service_token = volume_name.starts_with("kube-api-access-")
                && path == "/var/run/secrets/kubernetes.io/serviceaccount"
                && mount.get("readOnly").and_then(Value::as_bool) == Some(true);
            if !service_token
                && (volume.get("configMap").is_some()
                    || volume.get("secret").is_some()
                    || volume.get("projected").is_some())
            {
                return Err(invalid("unresolved mounted configuration dependency"));
            }
        }
    }
    normalized_pod(pod)
}

pub(crate) fn normalized_pod(pod: &Value) -> Result<Value> {
    // Names and scheduling of individual replicas are not runtime configuration.
    // Normalize generated service-account volume names.
    let mut normalized = pod
        .pointer("/spec")
        .cloned()
        .ok_or_else(|| invalid("pod spec missing"))?;
    let spec = normalized
        .as_object_mut()
        .ok_or_else(|| invalid("invalid pod spec"))?;
    for key in [
        "nodeName",
        "hostname",
        "subdomain",
        "serviceAccount",
        "serviceAccountName",
        "priority",
        "priorityClassName",
        "preemptionPolicy",
    ] {
        spec.remove(key);
    }
    if let Some(volumes) = spec.get_mut("volumes").and_then(Value::as_array_mut) {
        volumes.retain(|v| {
            !v.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.starts_with("kube-api-access-"))
        });
    }
    for key in ["containers", "initContainers"] {
        if let Some(items) = spec.get_mut(key).and_then(Value::as_array_mut) {
            for item in items {
                if let Some(mounts) = item.get_mut("volumeMounts").and_then(Value::as_array_mut) {
                    mounts.retain(|m| {
                        !m.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| n.starts_with("kube-api-access-"))
                    });
                }
            }
        }
    }
    Ok(normalized)
}

/// Validate the persisted engine projection. Secret-bearing, unknown or
/// dynamically resolved configuration is deliberately not representable.
pub fn validate_engine_spec(engine: &Value) -> Result<()> {
    let obj = engine
        .as_object()
        .ok_or_else(|| invalid("engine spec is not an object"))?;
    if obj.keys().any(|k| {
        ![
            "image",
            "command",
            "args",
            "env",
            "resources",
            "securityContext",
            "volumeMounts",
        ]
        .contains(&k.as_str())
    }) {
        return Err(invalid("unknown persisted engine field"));
    }
    pinned_image(string(engine, "/image")?)?;
    let command = array(engine, "/command")?;
    if command.len() != 3
        || command[0].as_str() != Some("vllm")
        || command[1].as_str() != Some("serve")
        || command[2].as_str() != Some("/opt/thalamus/model")
    {
        return Err(invalid("unsupported engine command"));
    }
    let args = array(engine, "/args")?;
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i]
            .as_str()
            .ok_or_else(|| invalid("non-string engine argument"))?;
        if !seen.insert(flag) {
            return Err(invalid("duplicate engine argument"));
        }
        match flag {
            "--enable-prefix-caching"
            | "--no-enable-prefix-caching"
            | "--enable-auto-tool-choice" => {
                i += 1;
            }
            "--max-model-len"
            | "--max-num-seqs"
            | "--max-num-batched-tokens"
            | "--tensor-parallel-size"
            | "--kv-offloading-size" => {
                let value = args
                    .get(i + 1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("missing numeric engine argument"))?;
                if value.is_empty()
                    || value.len() > 10
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(invalid("invalid numeric engine argument"));
                }
                i += 2;
            }
            "--served-model-name" => {
                let value = args
                    .get(i + 1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("missing served model name"))?;
                if value.is_empty()
                    || value.len() > 63
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return Err(invalid("unsafe served model name"));
                }
                i += 2;
            }
            "--tokenizer" => {
                if args.get(i + 1).and_then(Value::as_str) != Some("/opt/thalamus/model") {
                    return Err(invalid("invalid tokenizer argument"));
                }
                i += 2;
            }
            "--kv-offloading-backend" => {
                if args.get(i + 1).and_then(Value::as_str) != Some("native") {
                    return Err(invalid("unsupported offloading backend"));
                }
                i += 2;
            }
            "--scheduling-policy" => {
                if args.get(i + 1).and_then(Value::as_str) != Some("priority") {
                    return Err(invalid("unsupported scheduling policy"));
                }
                i += 2;
            }
            "--tool-call-parser" | "--reasoning-parser" => {
                let value = args
                    .get(i + 1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("missing parser"))?;
                if ![
                    "hermes",
                    "mistral",
                    "llama3_json",
                    "granite",
                    "granite-20b-fc",
                    "deepseek_v3",
                    "deepseek_r1",
                    "deepseek_r1_streaming",
                    "qwen3",
                    "qwen3_coder",
                    "gptoss",
                    "json",
                    "pythonic",
                ]
                .contains(&value)
                {
                    return Err(invalid("unreviewed parser name"));
                }
                i += 2;
            }
            _ => return Err(invalid("unreviewed engine argument")),
        }
    }
    if !seen.contains("--tokenizer")
        || !seen.contains("--served-model-name")
        || (seen.contains("--enable-prefix-caching") && seen.contains("--no-enable-prefix-caching"))
    {
        return Err(invalid("incomplete or conflicting engine arguments"));
    }
    if let Some(env) = engine.get("env") {
        let mut names = BTreeSet::new();
        for e in env
            .as_array()
            .ok_or_else(|| invalid("invalid engine environment"))?
        {
            let name = string(e, "/name")?;
            let expected = match name {
                "HOME" => "/tmp",
                "HF_HOME" => "/tmp/huggingface",
                "HF_HUB_OFFLINE" | "TRANSFORMERS_OFFLINE" => "1",
                _ => return Err(invalid("unreviewed engine environment")),
            };
            if e.as_object().is_none_or(|o| o.len() != 2)
                || string(e, "/value")? != expected
                || !names.insert(name)
            {
                return Err(invalid("invalid engine environment value"));
            }
        }
    }
    if let Some(mounts) = engine.get("volumeMounts") {
        for m in mounts
            .as_array()
            .ok_or_else(|| invalid("invalid engine mounts"))?
        {
            let n = string(m, "/name")?;
            let path = string(m, "/mountPath")?;
            if !matches!((n, path), ("shm", "/dev/shm") | ("tmp", "/tmp"))
                || m.as_object().is_none_or(|o| {
                    o.keys()
                        .any(|k| !["name", "mountPath", "readOnly"].contains(&k.as_str()))
                })
            {
                return Err(invalid("unreviewed engine mount"));
            }
        }
    }
    // These objects are included only after restricting their vocabulary and
    // values to resource quantities and Kubernetes security primitives.
    if let Some(resources) = engine.get("resources") {
        let resource_obj = resources
            .as_object()
            .ok_or_else(|| invalid("invalid resource requirements"))?;
        if resource_obj
            .keys()
            .any(|k| k != "requests" && k != "limits")
        {
            return Err(invalid("unreviewed resource field"));
        }
        for values in resource_obj.values() {
            for (key, value) in values
                .as_object()
                .ok_or_else(|| invalid("invalid resource map"))?
            {
                if key.len() > 128
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"./-_".contains(&b))
                    || value.as_str().is_none_or(|v| {
                        v.len() > 32
                            || !v
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
                    })
                {
                    return Err(invalid("invalid resource quantity"));
                }
            }
        }
    }
    if let Some(sc) = engine.get("securityContext") {
        let obj = sc
            .as_object()
            .ok_or_else(|| invalid("invalid security context"))?;
        for (key, value) in obj {
            match key.as_str() {
                "readOnlyRootFilesystem"
                | "allowPrivilegeEscalation"
                | "runAsNonRoot"
                | "privileged" => {
                    if !value.is_boolean() {
                        return Err(invalid("invalid security boolean"));
                    }
                }
                "runAsUser" | "runAsGroup" => {
                    if value.as_u64().is_none() {
                        return Err(invalid("invalid security uid"));
                    }
                }
                "capabilities" => {
                    let capabilities = value
                        .as_object()
                        .ok_or_else(|| invalid("invalid capabilities"))?;
                    if capabilities.keys().any(|k| k != "drop")
                        || capabilities
                            .get("drop")
                            .and_then(Value::as_array)
                            .is_none_or(|a| a.iter().any(|v| v.as_str() != Some("ALL")))
                    {
                        return Err(invalid("unreviewed capabilities"));
                    }
                }
                _ => return Err(invalid("unreviewed security context")),
            }
        }
    }
    Ok(())
}

fn engine_projection(normalized: &Value) -> Result<Value> {
    let engine = unique_named(array(normalized, "/containers")?, "main")?;
    let mut result = serde_json::Map::new();
    for key in [
        "image",
        "command",
        "args",
        "env",
        "resources",
        "securityContext",
        "volumeMounts",
    ] {
        if let Some(v) = engine.get(key) {
            result.insert(key.to_owned(), v.clone());
        }
    }
    let value = Value::Object(result);
    validate_engine_spec(&value)?;
    Ok(value)
}

pub(crate) fn gateway_images(r: &LiveResources, route: &Value) -> Result<BTreeMap<String, String>> {
    let mut components = BTreeMap::new();
    for parent in array(route, "/spec/parentRefs")? {
        let (ns, gateway_name) = parent_key(parent, namespace(route)?)?;
        let gateway = lookup(&r.gateways, &ns, &gateway_name)?;
        let labels = BTreeMap::from([(
            "gateway.networking.k8s.io/gateway-name".to_owned(),
            gateway_name.clone(),
        )]);
        let mut common: Option<BTreeMap<String, String>> = None;
        for pod in r.pods.iter().filter(|pod| {
            namespace(pod).ok() == Some(ns.as_str())
                && selected(pod, &labels)
                && pod_condition(pod, "Ready")
                && pod.pointer("/metadata/deletionTimestamp").is_none()
        }) {
            owner_chain(r, pod, gateway, false)?;
            if !pod_condition(pod, "Initialized")
                || pod.pointer("/status/phase").and_then(Value::as_str) != Some("Running")
            {
                return Err(invalid("gateway pod is not initialized and running"));
            }
            let ids = image_ids(pod, true)?;
            for (spec_path, prefix) in [
                ("/spec/containers", "container"),
                ("/spec/initContainers", "init"),
            ] {
                for container in pod
                    .pointer(spec_path)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let container_name = string(container, "/name")?;
                    let pinned = pinned_image(string(container, "/image")?)?;
                    if ids.get(&format!("{prefix}/{container_name}")) != Some(&pinned) {
                        return Err(invalid(
                            "gateway actual imageID differs from its immutable image pin",
                        ));
                    }
                }
            }
            if common.as_ref().is_some_and(|previous| previous != &ids) {
                return Err(invalid("gateway pods have mixed images"));
            }
            common = Some(ids);
        }
        let common = common.ok_or_else(|| invalid("no observed Ready gateway pods"))?;
        for (key, value) in common {
            let key = format!("gateway/{ns}/{gateway_name}/{key}");
            if components.insert(key, value).is_some() {
                return Err(invalid("duplicate gateway parent"));
            }
        }
    }
    Ok(components)
}

fn picker_evidence(
    r: &LiveResources,
    pool: &Value,
) -> Result<(Value, Value, BTreeMap<String, String>)> {
    let ns = namespace(pool)?;
    let picker = lookup(&r.picker_services, ns, picker_name(pool)?)?;
    if picker
        .pointer("/spec/publishNotReadyAddresses")
        .and_then(Value::as_bool)
        == Some(true)
        || array(picker, "/spec/ports")?.is_empty()
    {
        return Err(invalid("picker Service has no Ready-only endpoints"));
    }
    let reference = pool
        .pointer("/spec/endpointPickerRef")
        .or_else(|| pool.pointer("/spec/extensionRef"))
        .ok_or_else(|| invalid("missing picker reference"))?;
    let picker_port = reference
        .pointer("/port/number")
        .and_then(Value::as_u64)
        .or_else(|| reference.get("port").and_then(Value::as_u64))
        .ok_or_else(|| invalid("picker reference port missing"))?;
    let mappings: Vec<_> = array(picker, "/spec/ports")?
        .iter()
        .filter(|p| {
            p.get("port").and_then(Value::as_u64) == Some(picker_port)
                && p.get("protocol").and_then(Value::as_str).unwrap_or("TCP") == "TCP"
        })
        .collect();
    if mappings.len() != 1 {
        return Err(invalid("picker Service port is missing or ambiguous"));
    }
    let target_port = mappings[0]
        .get("targetPort")
        .unwrap_or(&mappings[0]["port"]);
    let mut picker_policies = Vec::new();
    for policy in &r.policies {
        let targets = policy.pointer("/spec/targetRefs").and_then(Value::as_array);
        if namespace(policy).ok() == Some(ns)
            && targets.is_some_and(|targets| {
                targets.iter().any(|target| {
                    target.get("kind").and_then(Value::as_str) == Some("Service")
                        && target.get("name").and_then(Value::as_str) == name(picker).ok()
                })
            })
        {
            let ancestors = array(policy, "/status/ancestors")?;
            if ancestors.len() != 1 {
                return Err(invalid("ambiguous picker policy status"));
            }
            condition(
                array(&ancestors[0], "/conditions")?,
                "Accepted",
                generation(policy)?,
            )?;
            picker_policies.push(
                policy
                    .get("spec")
                    .ok_or_else(|| invalid("picker policy spec missing"))?,
            );
        }
    }
    if picker_policies.len() > 1 {
        return Err(invalid("ambiguous picker policies"));
    }
    let labels = service_selector(picker)?;
    let mut common_ids: Option<BTreeMap<String, String>> = None;
    let mut common_spec: Option<Value> = None;
    for pod in r.pods.iter().filter(|p| {
        namespace(p).ok() == Some(ns) && selected(p, &labels) && pod_condition(p, "Ready")
    }) {
        if pod.pointer("/metadata/deletionTimestamp").is_some()
            || !pod_condition(pod, "Initialized")
            || pod.pointer("/status/phase").and_then(Value::as_str) != Some("Running")
        {
            return Err(invalid("picker pod is not a stable Ready endpoint"));
        }
        let owners: Vec<_> = r
            .workloads
            .iter()
            .filter(|d| {
                namespace(d).ok() == Some(ns)
                    && d.get("kind").and_then(Value::as_str) == Some("Deployment")
                    && owner_chain(r, pod, d, false).is_ok()
            })
            .collect();
        if owners.len() != 1 {
            return Err(invalid("picker pod controller ownership is ambiguous"));
        }
        rollout(owners[0], false)?;
        let ids = image_ids(pod, true)?;
        let exposes_target = array(pod, "/spec/containers")?.iter().any(|container| {
            container
                .get("ports")
                .and_then(Value::as_array)
                .is_some_and(|ports| {
                    ports.iter().any(|port| {
                        let protocol = port
                            .get("protocol")
                            .and_then(Value::as_str)
                            .unwrap_or("TCP");
                        protocol == "TCP"
                            && (target_port.as_u64().is_some_and(|n| {
                                port.get("containerPort").and_then(Value::as_u64) == Some(n)
                            }) || target_port.as_str().is_some_and(|n| {
                                port.get("name").and_then(Value::as_str) == Some(n)
                            }))
                    })
                })
        });
        if !exposes_target {
            return Err(invalid(
                "picker Pod does not expose the Service target port",
            ));
        }
        for container in array(pod, "/spec/containers")? {
            if container.get("readinessProbe").is_none() {
                return Err(invalid("picker container has no readiness probe"));
            }
            if container
                .get("envFrom")
                .and_then(Value::as_array)
                .is_some_and(|e| !e.is_empty())
                || container
                    .get("env")
                    .and_then(Value::as_array)
                    .is_some_and(|e| e.iter().any(|v| v.get("valueFrom").is_some()))
            {
                return Err(invalid("unresolved picker environment dependency"));
            }
            for mount in container
                .get("volumeMounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let volume_name = string(mount, "/name")?;
                let volume = array(pod, "/spec/volumes")?
                    .iter()
                    .find(|v| v.get("name").and_then(Value::as_str) == Some(volume_name))
                    .ok_or_else(|| invalid("missing picker volume"))?;
                let service_token = volume_name.starts_with("kube-api-access-")
                    && mount.get("mountPath").and_then(Value::as_str)
                        == Some("/var/run/secrets/kubernetes.io/serviceaccount")
                    && mount.get("readOnly").and_then(Value::as_bool) == Some(true);
                if !service_token
                    && (volume.get("secret").is_some()
                        || volume.get("configMap").is_some()
                        || volume.get("projected").is_some())
                {
                    return Err(invalid("unresolved picker configuration mount"));
                }
            }
        }
        let spec = normalized_pod(pod)?;
        if common_ids.as_ref().is_some_and(|v| v != &ids)
            || common_spec.as_ref().is_some_and(|v| v != &spec)
        {
            return Err(invalid(
                "picker endpoints have mixed image or configuration",
            ));
        }
        common_ids = Some(ids);
        common_spec = Some(spec);
    }
    let ids = common_ids.ok_or_else(|| invalid("no Ready picker endpoint"))?;
    let config = common_spec.ok_or_else(|| invalid("picker configuration missing"))?;
    let components = ids
        .into_iter()
        .map(|(key, value)| {
            (
                format!("picker/{}/{key}", name(picker).unwrap_or("")),
                value,
            )
        })
        .collect();
    Ok((
        picker
            .get("spec")
            .cloned()
            .ok_or_else(|| invalid("picker service spec missing"))?,
        json!({"pod":config,"policies":picker_policies}),
        components,
    ))
}

pub(crate) fn join_backend(
    config: &RuntimeConfig,
    service: &str,
    backend_name: &str,
    route: &Value,
    r: &LiveResources,
) -> Result<DeploymentSnapshot> {
    let backend = lookup(&r.backends, &config.namespace, backend_name)?;
    let deployment = annotation(backend, "thalamus.ai/deployment")?;
    let revision = annotation(backend, "thalamus.ai/revision")?;
    if revision.len() != 64
        || !revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "deployment revision is not immutable lowercase sha256",
        ));
    }
    let raw = annotation(backend, "thalamus.ai/serving-kind").ok() == Some("raw");
    if backend
        .pointer("/spec/ai/provider/custom/model")
        .and_then(Value::as_str)
        != Some(service)
    {
        return Err(invalid("backend model does not match service"));
    }
    let provider = backend
        .pointer("/spec/ai/provider")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("missing backend provider"))?;
    if provider.len() != 1 || !provider.contains_key("custom") {
        return Err(invalid("ambiguous backend provider"));
    }
    let formats = array(backend, "/spec/ai/provider/custom/formats")?;
    let chat_only = formats.as_slice()
        == [json!({"type": "Completions", "path": "/v1/chat/completions"})]
        && backend.pointer("/spec/policies/ai").is_none();
    let agentic_formats = formats.as_slice()
        == [
            json!({"type": "Completions", "path": "/v1/chat/completions"}),
            json!({"type": "Responses", "path": "/v1/responses"}),
            json!({"type": "Messages", "path": "/v1/messages"}),
        ]
        && backend.pointer("/spec/policies/ai")
            == Some(&json!({"routes": {
                "/v1/chat/completions": "Completions",
                "/v1/responses": "Responses",
                "/v1/messages": "Messages"
            }}));
    if !chat_only && !agentic_formats {
        return Err(invalid(
            "backend does not use the reviewed inference formats",
        ));
    }
    let pool_ref = backend
        .pointer("/spec/ai/provider/custom/backendRef")
        .ok_or_else(|| invalid("missing pool reference"))?;
    if string(pool_ref, "/group")? != "inference.networking.k8s.io"
        || string(pool_ref, "/kind")? != "InferencePool"
        || pool_ref
            .get("namespace")
            .and_then(Value::as_str)
            .is_some_and(|n| n != config.namespace)
    {
        return Err(invalid("unsupported pool reference"));
    }
    let pool = lookup(&r.pools, &config.namespace, string(pool_ref, "/name")?)?;
    let sets = array(backend, "/spec/policies/transformation/response/set")?;
    let response = backend
        .pointer("/spec/policies/transformation/response")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("backend response policy missing"))?;
    if response.len() != 1 || !response.contains_key("set") || sets.len() != 1 {
        return Err(invalid("backend response policy is ambiguous"));
    }
    let header_values: Vec<_> = sets
        .iter()
        .filter(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case("x-thalamus-backend"))
        })
        .collect();
    if header_values.len() != 1 {
        return Err(invalid("missing or ambiguous backend response header"));
    }
    let header: String = serde_json::from_str(string(header_values[0], "/value")?)
        .map_err(|_| invalid("backend response identity is not a CEL literal"))?;
    if header != format!("{service}/{deployment}/{revision}") {
        return Err(invalid("backend header does not bind revision"));
    }
    let serving = lookup(&r.serving, &config.namespace, deployment)?;
    let serving_kind = string(serving, "/kind")?;
    if (raw && serving_kind != "Deployment")
        || (!raw && serving_kind != "LLMInferenceService")
        || annotation(serving, "thalamus.ai/revision")? != revision
    {
        return Err(invalid("serving resource and backend revision disagree"));
    }
    if !raw {
        ready(serving)?;
    }
    let (flux, flux_dependency) = flux_revision(r, serving, backend, pool, raw)?;
    let manifest_name = annotation(serving, "thalamus.ai/model-manifest")?;
    let manifest_map = lookup(&r.config_maps, &config.namespace, manifest_name)?;
    if manifest_map.get("immutable").and_then(Value::as_bool) != Some(true) {
        return Err(invalid("model manifest ConfigMap is mutable"));
    }
    let manifest: Value = serde_json::from_str(string(manifest_map, "/data/manifest.json")?)?;
    let image = string(&manifest, "/image")?;
    let expected_image_digest = pinned_image(image)?;
    let model_path = string(&manifest, "/modelPath")?;
    if model_path != "/opt/thalamus/model" {
        return Err(invalid("unreviewed curated model path"));
    }
    let model = content_digest(string(&manifest, "/modelDigest")?)?;
    let tokenizer = content_digest(string(&manifest, "/tokenizerDigest")?)?;
    let labels = selector(pool)?;
    let target_port = pool_port(pool)?;
    let mut nodes = Vec::new();
    let mut common_ids: Option<BTreeMap<String, String>> = None;
    let mut common_spec: Option<Value> = None;
    let filler_name = format!("{deployment}-filler");
    let filler = r.serving.iter().find(|s| {
        namespace(s).ok() == Some(config.namespace.as_str())
            && name(s).ok() == Some(filler_name.as_str())
    });
    if let Some(filler) = filler {
        let (filler_flux, filler_dependency) = flux_revision(r, filler, backend, pool, true)?;
        if !raw
            || string(filler, "/kind")? != "Deployment"
            || annotation(filler, "thalamus.ai/deployment")? != deployment
            || annotation(filler, "thalamus.ai/revision")? != revision
            || annotation(filler, "thalamus.ai/model-manifest")? != manifest_name
            || filler_flux != flux
            || filler_dependency != flux_dependency
        {
            return Err(invalid(
                "filler workload is not bound to the same revision and manifest",
            ));
        }
        let desired = filler
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        if desired == 0 {
            if filler
                .pointer("/status/observedGeneration")
                .and_then(Value::as_i64)
                != Some(generation(filler)?)
                || filler
                    .pointer("/status/replicas")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    != 0
            {
                return Err(invalid("zero-replica filler rollout is not current"));
            }
        } else {
            rollout(filler, true)?;
        }
    }
    for pod in r
        .pods
        .iter()
        .filter(|p| namespace(p).ok() == Some(config.namespace.as_str()) && selected(p, &labels))
    {
        if !pod_condition(pod, "Ready") {
            continue;
        }
        if pod.pointer("/metadata/deletionTimestamp").is_some()
            || !pod_condition(pod, "Initialized")
            || pod.pointer("/status/phase").and_then(Value::as_str) != Some("Running")
        {
            return Err(invalid("selected pod is not a stable Ready endpoint"));
        }
        let workload = match owner_chain(r, pod, serving, raw) {
            Ok(v) => v,
            Err(_) => match filler {
                Some(filler) => owner_chain(r, pod, filler, true)?,
                None => return Err(invalid("selected pod is not owned by serving workload")),
            },
        };
        let ids = image_ids(pod, true)?;
        if ids.get("container/main") != Some(&expected_image_digest) {
            return Err(invalid(
                "actual engine imageID does not match curated OCI digest",
            ));
        }
        check_node(r, pod)?;
        let main = unique_named(array(pod, "/spec/containers")?, "main")?;
        if !array(main, "/ports")?.iter().any(|port| {
            port.get("containerPort").and_then(Value::as_u64) == Some(target_port)
                && port
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("TCP")
                    == "TCP"
        }) {
            return Err(invalid(
                "pool target port is not exposed by the serving engine",
            ));
        }
        let spec = engine_configuration(pod, image, model_path)?;
        let projected = engine_projection(&spec)?;
        let model_argument = array(&projected, "/args")?
            .windows(2)
            .find(|w| w[0].as_str() == Some("--served-model-name"))
            .and_then(|w| w[1].as_str());
        if model_argument != Some(service) {
            return Err(invalid(
                "engine served-model-name differs from backend model",
            ));
        }
        if let Some(workload) = workload {
            let template =
                unique_named(array(workload, "/spec/template/spec/containers")?, "main")?;
            let actual = unique_named(array(pod, "/spec/containers")?, "main")?;
            for field in [
                "image",
                "command",
                "args",
                "env",
                "securityContext",
                "resources",
            ] {
                if template.get(field).is_some() && template.get(field) != actual.get(field) {
                    return Err(invalid(format!(
                        "pod {field} differs from current workload template"
                    )));
                }
            }
        }
        if common_ids.as_ref().is_some_and(|v| v != &ids)
            || common_spec.as_ref().is_some_and(|v| v != &spec)
        {
            return Err(invalid(
                "eligible pods have mixed images or effective configuration",
            ));
        }
        common_ids = Some(ids);
        common_spec = Some(spec);
        nodes.push(string(pod, "/spec/nodeName")?.to_owned());
    }
    nodes.sort();
    let image_digests = common_ids.ok_or_else(|| invalid("pool has no Ready eligible pods"))?;
    let effective = common_spec.ok_or_else(|| invalid("missing pod configuration"))?;
    let engine_spec = engine_projection(&effective)?;
    let gateway = gateway_images(r, route)?;
    let (picker_service, picker_config, picker_components) = picker_evidence(r, pool)?;
    let mut component_versions = gateway;
    component_versions.extend(picker_components);
    component_versions.insert("engine".to_owned(), expected_image_digest);
    let mut normalized_serving = serving
        .get("spec")
        .cloned()
        .ok_or_else(|| invalid("serving spec missing"))?;
    let normalized_object = normalized_serving
        .as_object_mut()
        .ok_or_else(|| invalid("serving spec is not an object"))?;
    normalized_object.remove("replicas");
    normalized_object.remove("role");
    let rollout_configuration = canonical_digest(&json!({
        "serving": normalized_serving, "backend": backend.get("spec"), "pool": pool.get("spec"),
        "picker": &picker_service, "manifest": &manifest,
        "chart_digest": flux_dependency.get("digest"), "chart_revision": flux_dependency.get("revision"),
    }))?;
    let desired = DesiredIdentity {
        rendered_resources: canonical_digest(
            &json!({"serving": serving.get("spec"), "backend": backend.get("spec"), "pool": pool.get("spec"), "picker": &picker_service, "filler": filler.and_then(|f| f.get("spec"))}),
        )?,
        resolved_dependencies: canonical_digest(
            &json!({"manifest": &manifest, "flux_chart": &flux_dependency}),
        )?,
    };
    let runtime = RuntimeIdentity {
        image_digests,
        model_content: model,
        tokenizer_content: tokenizer,
        effective_configuration: canonical_digest(
            &json!({"engine": effective, "picker": picker_config}),
        )?,
        component_versions,
    };
    Ok(DeploymentSnapshot {
        deployment: deployment.to_owned(),
        revision: revision.to_owned(),
        flux_revision: flux,
        backend: header,
        desired,
        rollout_configuration,
        runtime,
        underlying_model: None,
        engine_spec,
        nodes,
    })
}

/// Join one live Job and all pods listed in its namespace. Failed and succeeded
/// pods are both valid provenance; neither a request nor a Job template is the
/// source of the observed image ID.
pub fn join_runner(job: &Value, pods: &[Value]) -> Result<RunnerObservation> {
    let job_name = name(job)?;
    let job_uid = uid(job)?;
    let ns = namespace(job)?;
    let mut owned = Vec::new();
    for pod in pods {
        if namespace(pod)? != ns {
            continue;
        }
        let owners = pod
            .pointer("/metadata/ownerReferences")
            .and_then(Value::as_array);
        if owners.is_some_and(|owners| {
            owners.iter().any(|o| {
                o.get("controller").and_then(Value::as_bool) == Some(true)
                    && o.get("kind").and_then(Value::as_str) == Some("Job")
                    && o.get("uid").and_then(Value::as_str) == Some(job_uid)
            })
        }) {
            owned.push(pod);
        }
    }
    if owned.len() != 1 {
        return Err(invalid(
            "runner Job must have exactly one observed owned pod",
        ));
    }
    let pod = owned[0];
    let owner = controller_owner(pod)?;
    if string(owner, "/kind")? != "Job"
        || string(owner, "/name")? != job_name
        || string(owner, "/uid")? != job_uid
    {
        return Err(invalid("runner owner reference does not match Job"));
    }
    let runner = unique_named(array(pod, "/spec/containers")?, "runner")?;
    let status = unique_named(array(pod, "/status/containerStatuses")?, "runner")?;
    let image = string(runner, "/image")?;
    pinned_image(image)?;
    let image_id = digest(string(status, "/imageID")?)?;
    Ok(RunnerObservation {
        job: job_name.to_owned(),
        job_uid: job_uid.to_owned(),
        pod: name(pod)?.to_owned(),
        pod_uid: uid(pod)?.to_owned(),
        pod_owner_job_uid: string(owner, "/uid")?.to_owned(),
        image: image.to_owned(),
        image_id,
    })
}
