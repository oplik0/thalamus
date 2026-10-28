//! Fail-closed joins for the agentic hop in front of a directly observed model.
//!
//! A route's annotations and asserted response header are only discovery hints:
//! the static backend, Service, every eligible Pod, internal route, and the
//! existing model/pool join are independently checked before returning evidence.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::observe::{
    LiveResources, RoutePolicyMode, annotation, array, canonical_digest, check_route_policy,
    generation, image_ids, invalid, join_backend, join_route, lookup, name, namespace,
    normalized_pod, owner_chain, parent_key, pinned_image, pod_condition, rollout, route_listeners,
    selected, service_selector, string, uid,
};
use crate::{
    DeploymentSnapshot, Result, RouteListener, RouteSnapshot, RuntimeConfig,
    UnderlyingModelIdentity,
};

const CANDIDATE_PATHS: &[&str] = &["/v1/chat/completions", "/v1/responses", "/v1/messages"];
const CA_PATH: &str = "/var/run/agentic-ca";
const CA_FILE: &str = "/var/run/agentic-ca/ca.crt";

type Match = (String, String, String);

fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn dns_host(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(dns_label)
}

fn safe_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.chars().any(|c| matches!(c, '%' | '?' | '#' | '\\'))
        && !value.contains("//")
        && !value.split('/').any(|part| part == "." || part == "..")
}

fn alive(value: &Value) -> Result<()> {
    if value.pointer("/metadata/deletionTimestamp").is_some() {
        return Err(invalid("agentic resource is terminating"));
    }
    uid(value)?;
    Ok(())
}

fn resource_annotations(
    value: &Value,
    service: &str,
    instance: Option<&str>,
    team: Option<&str>,
    deployment: Option<&DeploymentSnapshot>,
) -> Result<()> {
    alive(value)?;
    if annotation(value, "thalamus.ai/service")? != service
        || instance
            .is_some_and(|s| annotation(value, "thalamus.ai/agentic-instance").ok() != Some(s))
        || team.is_some_and(|s| annotation(value, "thalamus.ai/team").ok() != Some(s))
    {
        return Err(invalid("agentic resource identity annotations disagree"));
    }
    if let Some(target) = deployment
        && (annotation(value, "thalamus.ai/deployment")? != target.deployment
            || annotation(value, "thalamus.ai/revision")? != target.revision)
    {
        return Err(invalid("agentic resource revision annotations disagree"));
    }
    Ok(())
}

fn hash_name(service: &str, instance: &str, deployment: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(service.as_bytes());
    hash.update(b"/");
    hash.update(instance.as_bytes());
    hash.update(b"/");
    hash.update(deployment.as_bytes());
    format!("{:x}", hash.finalize())[..32].to_owned()
}

fn route_hosts(route: &Value) -> Result<Vec<String>> {
    let mut hosts = BTreeSet::new();
    for host in array(route, "/spec/hostnames")? {
        let host = host
            .as_str()
            .filter(|s| dns_host(s))
            .ok_or_else(|| invalid("invalid agentic hostname"))?;
        if !hosts.insert(host.to_owned()) {
            return Err(invalid("duplicate agentic hostname"));
        }
    }
    if hosts.len() != 1 {
        return Err(invalid("agentic route must have one exact hostname"));
    }
    Ok(hosts.into_iter().collect())
}

fn tls_listeners(r: &LiveResources, route: &Value, hosts: &[String]) -> Result<Vec<RouteListener>> {
    let listeners = route_listeners(r, route, hosts)?;
    if listeners.is_empty() || listeners.iter().any(|l| l.protocol != "HTTPS") {
        return Err(invalid("agentic route is not exclusively HTTPS"));
    }
    for parent in array(route, "/spec/parentRefs")? {
        let (ns, gateway_name) = parent_key(parent, namespace(route)?)?;
        let gateway = lookup(&r.gateways, &ns, &gateway_name)?;
        alive(gateway)?;
        for listener in array(gateway, "/spec/listeners")? {
            if listener.get("protocol").and_then(Value::as_str) != Some("HTTPS")
                || parent
                    .get("sectionName")
                    .and_then(Value::as_str)
                    .is_some_and(|n| listener.get("name").and_then(Value::as_str) != Some(n))
                || parent
                    .get("port")
                    .and_then(Value::as_u64)
                    .is_some_and(|p| listener.get("port").and_then(Value::as_u64) != Some(p))
            {
                continue;
            }
            let listener_host = listener.get("hostname").and_then(Value::as_str);
            let matches_host = hosts.iter().any(|h| {
                listener_host.is_none_or(|p| {
                    p.eq_ignore_ascii_case(h)
                        || p.strip_prefix("*.").is_some_and(|suffix| {
                            h.strip_suffix(suffix)
                                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
                        })
                })
            });
            if !matches_host {
                continue;
            }
            let refs = array(listener, "/tls/certificateRefs")?;
            if listener
                .pointer("/tls/mode")
                .and_then(Value::as_str)
                .unwrap_or("Terminate")
                != "Terminate"
                || refs.len() != 1
                || refs[0]
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("Secret")
                    != "Secret"
                || !refs[0]
                    .get("group")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
                || refs[0]
                    .get("namespace")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n != ns)
                || !dns_host(string(&refs[0], "/name")?)
            {
                return Err(invalid(
                    "agentic listener has no unique same-namespace TLS certificate",
                ));
            }
        }
    }
    Ok(listeners)
}

// Record the configuration and UIDs of every eligible Gateway Pod, not just
// its shared image digest. A stage comparison can ignore Pod replacement while
// full evidence still changes when any selected proxy instance is replaced.
fn gateway_evidence(
    r: &LiveResources,
    route: &Value,
) -> Result<(BTreeMap<String, String>, Value, Value)> {
    let mut components = BTreeMap::new();
    let mut configurations = Vec::new();
    let mut identities = Vec::new();
    for parent in array(route, "/spec/parentRefs")? {
        let (ns, gateway_name) = parent_key(parent, namespace(route)?)?;
        let gateway = lookup(&r.gateways, &ns, &gateway_name)?;
        alive(gateway)?;
        let labels = BTreeMap::from([(
            "gateway.networking.k8s.io/gateway-name".to_owned(),
            gateway_name.clone(),
        )]);
        let pods = serving_pods(r, &ns, &labels, gateway)?;
        let mut common_ids: Option<BTreeMap<String, String>> = None;
        let mut common_spec: Option<Value> = None;
        let mut uids = Vec::new();
        for pod in pods {
            let ids = image_ids(pod, true)?;
            for (path, prefix) in [
                ("/spec/containers", "container"),
                ("/spec/initContainers", "init"),
            ] {
                for container in pod
                    .pointer(path)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let key = format!("{prefix}/{}", string(container, "/name")?);
                    if ids.get(&key) != Some(&pinned_image(string(container, "/image")?)?) {
                        return Err(invalid("Gateway Pod imageID is not its pinned image"));
                    }
                }
            }
            let spec = normalized_pod(pod)?;
            if common_ids.as_ref().is_some_and(|v| v != &ids)
                || common_spec.as_ref().is_some_and(|v| v != &spec)
            {
                return Err(invalid(
                    "Gateway Pods have mixed image or effective configuration",
                ));
            }
            common_ids = Some(ids);
            common_spec = Some(spec);
            uids.push(uid(pod)?.to_owned());
        }
        let ids = common_ids.ok_or_else(|| invalid("Gateway has no Ready Pods"))?;
        for (key, value) in ids {
            if components
                .insert(format!("gateway/{ns}/{gateway_name}/{key}"), value)
                .is_some()
            {
                return Err(invalid("duplicate agentic Gateway parent"));
            }
        }
        configurations.push(json!({"namespace":ns,"name":gateway_name,"gateway":gateway.get("spec"),"pod":common_spec}));
        identities.push(
            json!({"namespace":ns,"name":gateway_name,"gateway_uid":uid(gateway)?,"pod_uids":uids}),
        );
    }
    Ok((
        components,
        Value::Array(configurations),
        Value::Array(identities),
    ))
}

fn matches_overlap(a: &Match, b: &Match) -> bool {
    if a.0 != b.0 {
        return false;
    }
    let beneath = |child: &str, parent: &str| {
        parent == "/"
            || child == parent
            || child
                .strip_prefix(parent.trim_end_matches('/'))
                .is_some_and(|rest| rest.starts_with('/'))
    };
    match (a.1.as_str(), b.1.as_str()) {
        ("Exact", "Exact") => a.2 == b.2,
        ("PathPrefix", "Exact") => beneath(&b.2, &a.2),
        ("Exact", "PathPrefix") => beneath(&a.2, &b.2),
        ("PathPrefix", "PathPrefix") => beneath(&a.2, &b.2) || beneath(&b.2, &a.2),
        _ => true,
    }
}

// Treat an unknown or malformed competing route as overlapping. Gateway route
// precedence is not evidence that a different configuration cannot win.
fn competing(
    r: &LiveResources,
    route: &Value,
    hosts: &[String],
    own: &BTreeSet<Match>,
) -> Result<()> {
    let ns = namespace(route)?;
    let parents: BTreeSet<_> = array(route, "/spec/parentRefs")?
        .iter()
        .map(|p| parent_key(p, ns))
        .collect::<Result<_>>()?;
    for other in &r.routes {
        let other_ns = namespace(other)?;
        if other_ns == ns && name(other)? == name(route)? {
            continue;
        }
        let Some(other_parents) = other.pointer("/spec/parentRefs").and_then(Value::as_array)
        else {
            continue;
        };
        if !other_parents
            .iter()
            .any(|p| parent_key(p, other_ns).is_ok_and(|p| parents.contains(&p)))
        {
            continue;
        }
        let host_overlap = other
            .pointer("/spec/hostnames")
            .and_then(Value::as_array)
            .is_none_or(|v| {
                v.is_empty()
                    || v.iter().any(|h| {
                        h.as_str().is_none_or(|h| {
                            hosts.iter().any(|ours| {
                                h.eq_ignore_ascii_case(ours)
                                    || h.strip_prefix("*.").is_some_and(|suffix| {
                                        ours.strip_suffix(suffix)
                                            .is_some_and(|p| p.ends_with('.') && p.len() > 1)
                                    })
                            })
                        })
                    })
            });
        if !host_overlap {
            continue;
        }
        let overlaps = other
            .pointer("/spec/rules")
            .and_then(Value::as_array)
            .is_none_or(|rules| {
                rules.is_empty()
                    || rules.iter().any(|rule| {
                        rule.get("matches")
                            .and_then(Value::as_array)
                            .is_none_or(|matches| {
                                matches.is_empty()
                                    || matches.iter().any(|m| {
                                        let method = m.get("method").and_then(Value::as_str);
                                        let path = m
                                            .pointer("/path/value")
                                            .and_then(Value::as_str)
                                            .unwrap_or("/");
                                        let kind = m
                                            .pointer("/path/type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("PathPrefix");
                                        if !safe_path(path)
                                            || !matches!(kind, "Exact" | "PathPrefix")
                                        {
                                            return true;
                                        }
                                        own.iter().any(|ours| {
                                            method.is_none_or(|v| v == ours.0)
                                                && matches_overlap(
                                                    ours,
                                                    &(
                                                        ours.0.clone(),
                                                        kind.to_owned(),
                                                        path.to_owned(),
                                                    ),
                                                )
                                        })
                                    })
                            })
                    })
            });
        if overlaps {
            return Err(invalid(format!(
                "competing agentic route {other_ns}/{}",
                name(other)?
            )));
        }
    }
    Ok(())
}

fn expected_public(team_shared: bool) -> BTreeSet<Match> {
    let mut matches: BTreeSet<_> = CANDIDATE_PATHS
        .iter()
        .map(|p| ("POST".into(), "Exact".into(), (*p).into()))
        .collect();
    matches.insert(("GET".into(), "PathPrefix".into(), "/v1/models".into()));
    if team_shared {
        for (method, kind, path) in [
            ("POST", "Exact", "/v1/responses/compact"),
            ("GET", "PathPrefix", "/v1/responses"),
            ("DELETE", "PathPrefix", "/v1/responses"),
            ("POST", "PathPrefix", "/v1/conversations"),
            ("GET", "PathPrefix", "/v1/conversations"),
            ("DELETE", "PathPrefix", "/v1/conversations"),
        ] {
            matches.insert((method.into(), kind.into(), path.into()));
        }
    }
    matches
}

fn backend_refs(rule: &Value, ns: &str, group: &str, kind: &str) -> Result<BTreeMap<String, u32>> {
    let mut result = BTreeMap::new();
    for reference in array(rule, "/backendRefs")? {
        let object = reference
            .as_object()
            .ok_or_else(|| invalid("invalid agentic backend reference"))?;
        if object
            .keys()
            .any(|k| !["group", "kind", "name", "namespace", "weight"].contains(&k.as_str()))
            || reference.get("group").and_then(Value::as_str) != Some(group)
            || reference.get("kind").and_then(Value::as_str) != Some(kind)
            || reference
                .get("namespace")
                .and_then(Value::as_str)
                .is_some_and(|v| v != ns)
        {
            return Err(invalid("unsafe agentic backend reference"));
        }
        let backend = string(reference, "/name")?;
        let weight = reference
            .get("weight")
            .map_or(Some(1), Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("invalid agentic weight"))?;
        if result.insert(backend.to_owned(), weight).is_some() {
            return Err(invalid("duplicate agentic backend reference"));
        }
    }
    if result.is_empty() {
        return Err(invalid("agentic route has no backends"));
    }
    Ok(result)
}

// Every rule must contain exactly the reviewed method/path match and, on the
// private and candidate routes, an exact path-preserving rewrite.
fn route_matches(
    route: &Value,
    expected: &BTreeSet<Match>,
    rewrite_prefix: Option<&str>,
    expected_refs: &BTreeMap<String, u32>,
    group: &str,
    kind: &str,
) -> Result<Vec<String>> {
    let mut actual = BTreeSet::new();
    let rules = array(route, "/spec/rules")?;
    if rules.is_empty() {
        return Err(invalid("agentic route has no rules"));
    }
    for rule in rules {
        let object = rule
            .as_object()
            .ok_or_else(|| invalid("invalid agentic rule"))?;
        if object
            .keys()
            .any(|k| !["name", "matches", "backendRefs", "filters"].contains(&k.as_str()))
            || backend_refs(rule, namespace(route)?, group, kind)? != *expected_refs
        {
            return Err(invalid("agentic rule changes its backend set"));
        }
        let matches = array(rule, "/matches")?;
        if matches.is_empty() {
            return Err(invalid("unbounded agentic rule"));
        }
        let rewrite = match rewrite_prefix {
            None => match rule.get("filters") {
                None => None,
                Some(Value::Array(filters)) if filters.is_empty() => None,
                _ => return Err(invalid("unreviewed agentic route filter")),
            },
            Some(_) => {
                let filters = array(rule, "/filters")?;
                if filters.len() != 1
                    || filters[0].get("type").and_then(Value::as_str) != Some("URLRewrite")
                    || filters[0].pointer("/urlRewrite/hostname").is_some()
                    || filters[0]
                        .pointer("/urlRewrite/path/type")
                        .and_then(Value::as_str)
                        != Some("ReplaceFullPath")
                    || filters[0]
                        .pointer("/urlRewrite")
                        .and_then(Value::as_object)
                        .is_none_or(|v| v.len() != 1)
                    || filters[0]
                        .pointer("/urlRewrite/path")
                        .and_then(Value::as_object)
                        .is_none_or(|v| v.len() != 2)
                {
                    return Err(invalid("unsupported agentic route rewrite"));
                }
                Some(string(&filters[0], "/urlRewrite/path/replaceFullPath")?)
            }
        };
        for m in matches {
            if m.as_object().is_none_or(|o| {
                o.len() != 2 || !o.contains_key("method") || !o.contains_key("path")
            }) || m
                .get("path")
                .and_then(Value::as_object)
                .is_none_or(|o| o.len() != 2 || !o.contains_key("type") || !o.contains_key("value"))
            {
                return Err(invalid("agentic match has additional conditions"));
            }
            let method = string(m, "/method")?;
            let kind = string(m, "/path/type")?;
            let path = string(m, "/path/value")?;
            if !safe_path(path) {
                return Err(invalid("unsafe agentic path"));
            }
            if let Some(prefix) = rewrite_prefix {
                let native = path
                    .strip_prefix(prefix)
                    .filter(|p| p.starts_with('/'))
                    .ok_or_else(|| invalid("agentic rewrite prefix mismatch"))?;
                if rewrite != Some(native) || kind != "Exact" {
                    return Err(invalid("agentic rewrite target mismatch"));
                }
            }
            if !actual.insert((method.to_owned(), kind.to_owned(), path.to_owned())) {
                return Err(invalid("duplicate agentic route match"));
            }
        }
    }
    if &actual != expected {
        return Err(invalid(
            "agentic route methods or paths differ from reviewed contract",
        ));
    }
    Ok(actual
        .into_iter()
        .map(|(_, _, path)| path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn route_policy<'a>(r: &'a LiveResources, route: &Value) -> Result<&'a Value> {
    let ns = namespace(route)?;
    let route_name = name(route)?;
    let mut policies = r.policies.iter().filter(|p| {
        namespace(p).ok() == Some(ns)
            && p.pointer("/spec/targetRefs")
                .and_then(Value::as_array)
                .is_some_and(|refs| {
                    refs.iter().any(|target| {
                        target.get("kind").and_then(Value::as_str) == Some("HTTPRoute")
                            && target.get("name").and_then(Value::as_str) == Some(route_name)
                    })
                })
    });
    let policy = policies
        .next()
        .ok_or_else(|| invalid("agentic route policy missing"))?;
    if policies.next().is_some() {
        return Err(invalid("ambiguous agentic route policy"));
    }
    alive(policy)?;
    Ok(policy)
}

// Candidate and stable private routes differ only in the deliberate job-key
// requirement. Preserve every other policy field in promotion comparison;
// desired identity separately includes each actual policy without normalization.
fn normalized_scope(policy: &Value, deployment: &str, candidate: bool) -> Result<Value> {
    let mut spec = policy
        .get("spec")
        .cloned()
        .ok_or_else(|| invalid("agentic policy spec missing"))?;
    let expression = format!(
        "apiKey.kind {} \"job\" {} apiKey.deployment == {}",
        if candidate { "==" } else { "!=" },
        if candidate { "&&" } else { "||" },
        serde_json::to_string(deployment)?
    );
    let expressions = spec
        .pointer_mut("/traffic/authorization/policy/matchExpressions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid("agentic authorization expressions missing"))?;
    if expressions
        .iter()
        .filter(|item| item.as_str() == Some(expression.as_str()))
        .count()
        != 1
        || expressions
            .iter()
            .any(|item| item.as_str() == Some("__thalamus_agentic_deployment_scope__"))
    {
        return Err(invalid("agentic deployment scope is missing or ambiguous"));
    }
    let position = expressions
        .iter()
        .position(|item| item.as_str() == Some(expression.as_str()))
        .ok_or_else(|| invalid("agentic deployment scope missing"))?;
    expressions[position] = Value::String("__thalamus_agentic_deployment_scope__".into());
    Ok(spec)
}

fn require_expression(policy: &Value, expression: &str) -> Result<()> {
    if array(
        policy,
        "/spec/traffic/authorization/policy/matchExpressions",
    )?
    .iter()
    .filter(|v| v.as_str() == Some(expression))
    .count()
        != 1
    {
        return Err(invalid(
            "reviewed agentic authorization guard missing or duplicated",
        ));
    }
    Ok(())
}

// Preserve the exact reviewed CEL body, while parsing its JSON literals rather
// than relying on a specific JSON whitespace encoding for origin/path lists.
fn check_outer_guards(
    policy: &Value,
    team: &str,
    candidate: Option<&str>,
) -> Result<BTreeSet<String>> {
    let prefix = candidate
        .map(|d| format!("/candidates/{d}"))
        .unwrap_or_default();
    let response = format!("{prefix}/v1/responses");
    if team == "shared" {
        let guard = format!(
            "request.path != {} || request.method != \"POST\" || json(request.body).with(b, type(b) == map && b.store == false && (!(\"previous_response_id\" in b) || b.previous_response_id == null) && (!(\"conversation\" in b) || b.conversation == null) && (!(\"conversation_id\" in b) || b.conversation_id == null))",
            serde_json::to_string(&response)?
        );
        require_expression(policy, &guard)?;
    } else {
        require_expression(
            policy,
            &format!("apiKey.team == {}", serde_json::to_string(team)?),
        )?;
        let guard = format!(
            "request.method != \"GET\" || (request.path != {} && request.path != {})",
            serde_json::to_string(&response)?,
            serde_json::to_string(&format!("{response}/"))?
        );
        require_expression(policy, &guard)?;
    }
    let expected_paths: Vec<_> = CANDIDATE_PATHS
        .iter()
        .map(|p| format!("{prefix}{p}"))
        .chain(
            candidate
                .is_none()
                .then_some("/v1/responses/compact".to_owned()),
        )
        .collect();
    const START: &str = "request.method != \"POST\" || !(request.path in ";
    const TOOL: &str = r##"type(t) == map && (!("type" in t) || t.type != "mcp" || ((!("server_url" in t) || t.server_url == null || (type(t.server_url) == string && allowed.exists(o, t.server_url == o || t.server_url.startsWith(o + "/") || t.server_url.startsWith(o + "?") || t.server_url.startsWith(o + "#")))) && (!("url" in t) || t.url == null || (type(t.url) == string && allowed.exists(o, t.url == o || t.url.startsWith(o + "/") || t.url.startsWith(o + "?") || t.url.startsWith(o + "#"))))))"##;
    const SERVERS: &str = r##"!("mcp_servers" in b) || b.mcp_servers == null || (type(b.mcp_servers) == list && b.mcp_servers.all(s, type(s) == map && "type" in s && s.type == "url" && "url" in s && type(s.url) == string && allowed.exists(o, s.url == o || s.url.startsWith(o + "/") || s.url.startsWith(o + "?") || s.url.startsWith(o + "#"))))"##;
    let body = format!(
        r#"json(request.body).with(b, type(b) == map && (!("tools" in b) || b.tools == null || (type(b.tools) == list && b.tools.all(t, {TOOL}))) && ({SERVERS})))"#
    );
    let mut matched = None;
    for expression in array(
        policy,
        "/spec/traffic/authorization/policy/matchExpressions",
    )? {
        let Some(rest) = expression.as_str().and_then(|s| s.strip_prefix(START)) else {
            continue;
        };
        let Some((paths, rest)) = rest.split_once(") || ") else {
            return Err(invalid("malformed MCP guard"));
        };
        let Some((origins, suffix)) = rest.split_once(".with(allowed, ") else {
            return Err(invalid("malformed MCP allowlist"));
        };
        let parsed_paths: Vec<String> =
            serde_json::from_str(paths).map_err(|_| invalid("invalid MCP guard paths"))?;
        if parsed_paths != expected_paths || suffix != body || matched.is_some() {
            return Err(invalid(
                "MCP guard does not protect exactly the actual incoming paths",
            ));
        }
        let origins: Vec<String> =
            serde_json::from_str(origins).map_err(|_| invalid("invalid MCP origin list"))?;
        let mut hosts = BTreeSet::new();
        for origin in origins {
            let url = url::Url::parse(&origin).map_err(|_| invalid("invalid MCP origin"))?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || origin.ends_with('/')
            {
                return Err(invalid("unsafe MCP origin"));
            }
            let host = match url
                .host()
                .ok_or_else(|| invalid("MCP origin has no host"))?
            {
                url::Host::Domain(domain) => {
                    if !dns_host(domain)
                        || !domain.contains('.')
                        || domain.ends_with(".localhost")
                        || domain
                            .rsplit('.')
                            .next()
                            .is_none_or(|part| !part.as_bytes()[0].is_ascii_lowercase())
                    {
                        return Err(invalid("unsafe MCP domain"));
                    }
                    domain.to_owned()
                }
                url::Host::Ipv4(ip) => {
                    let octets = ip.octets();
                    if octets[0] == 0
                        || octets[0] == 127
                        || (octets[0] == 169 && octets[1] == 254)
                        || octets[0] >= 224
                    {
                        return Err(invalid("unsafe MCP IPv4 address"));
                    }
                    ip.to_string()
                }
                url::Host::Ipv6(ip) => {
                    let octets = ip.octets();
                    if ip.is_unspecified()
                        || ip.is_loopback()
                        || ip.is_multicast()
                        || ip.is_unicast_link_local()
                        || (octets[..10] == [0; 10] && octets[10] == 0xff && octets[11] == 0xff)
                    {
                        return Err(invalid("unsafe MCP IPv6 address"));
                    }
                    ip.to_string()
                }
            };
            hosts.insert(host);
        }
        matched = Some(hosts);
    }
    matched.ok_or_else(|| invalid("reviewed MCP body guard missing"))
}

fn port(service: &Value, external: u64) -> Result<&Value> {
    let mut ports = array(service, "/spec/ports")?
        .iter()
        .filter(|p| p.get("port").and_then(Value::as_u64) == Some(external));
    let port = ports
        .next()
        .ok_or_else(|| invalid("agentic Service port missing"))?;
    if ports.next().is_some()
        || port
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("TCP")
            != "TCP"
    {
        return Err(invalid("ambiguous agentic Service port"));
    }
    Ok(port)
}

fn service_shape(service: &Value) -> Result<BTreeMap<String, String>> {
    alive(service)?;
    if service
        .pointer("/spec/type")
        .and_then(Value::as_str)
        .unwrap_or("ClusterIP")
        != "ClusterIP"
        || service.pointer("/spec/externalName").is_some()
        || service
            .pointer("/spec/externalIPs")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
        || service
            .pointer("/spec/publishNotReadyAddresses")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || service
            .pointer("/spec/clusterIP")
            .and_then(Value::as_str)
            .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
            .is_none_or(|ip| ip.is_unspecified() || ip.is_multicast())
    {
        return Err(invalid("agentic Service is not a Ready-endpoint ClusterIP"));
    }
    service_selector(service)
}

fn serving_pods<'a>(
    r: &'a LiveResources,
    ns: &str,
    labels: &BTreeMap<String, String>,
    owner: &'a Value,
) -> Result<Vec<&'a Value>> {
    let mut pods = Vec::new();
    let mut ids = BTreeSet::new();
    for pod in r
        .pods
        .iter()
        .filter(|p| namespace(p).ok() == Some(ns) && selected(p, labels))
    {
        if !pod_condition(pod, "Ready") {
            continue;
        }
        alive(pod)?;
        if !pod_condition(pod, "Initialized")
            || pod.pointer("/status/phase").and_then(Value::as_str) != Some("Running")
        {
            return Err(invalid("selected agentic Pod is not stably Ready"));
        }
        if !ids.insert(uid(pod)?.to_owned()) {
            return Err(invalid("duplicate selected agentic Pod UID"));
        }
        owner_chain(r, pod, owner, string(owner, "/kind")? == "Deployment")?;
        pods.push(pod);
    }
    if pods.is_empty() {
        return Err(invalid("agentic Service has no Ready owned Pods"));
    }
    pods.sort_by(|a, b| uid(a).unwrap_or("").cmp(uid(b).unwrap_or("")));
    Ok(pods)
}

fn target_port_matches(port: &Value, pod: &Value) -> Result<()> {
    let target = port
        .get("targetPort")
        .unwrap_or_else(|| port.get("port").unwrap());
    let mut found = 0;
    for container in array(pod, "/spec/containers")? {
        for p in container
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if p.get("protocol").and_then(Value::as_str).unwrap_or("TCP") != "TCP" {
                continue;
            }
            if target
                .as_u64()
                .is_some_and(|n| p.get("containerPort").and_then(Value::as_u64) == Some(n))
                || target
                    .as_str()
                    .is_some_and(|s| p.get("name").and_then(Value::as_str) == Some(s))
            {
                found += 1;
            }
        }
    }
    if found != 1 {
        return Err(invalid(
            "agentic Service targetPort is not uniquely exposed by its Pod",
        ));
    }
    Ok(())
}

fn static_backend(
    backend: &Value,
    host: &str,
    target: &DeploymentSnapshot,
    service: &str,
) -> Result<()> {
    let spec = backend
        .get("spec")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("missing static backend spec"))?;
    if spec.keys().any(|k| k != "static" && k != "policies")
        || backend.pointer("/spec/static/host").and_then(Value::as_str) != Some(host)
        || backend.pointer("/spec/static/port").and_then(Value::as_u64) != Some(9000)
        || backend
            .pointer("/spec/static")
            .and_then(Value::as_object)
            .is_none_or(|s| s.len() != 2)
    {
        return Err(invalid(
            "agentic static backend does not address its Service",
        ));
    }
    let policy = backend
        .pointer("/spec/policies")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("agentic backend identity policy missing"))?;
    if policy.len() != 1
        || !policy.contains_key("transformation")
        || backend
            .pointer("/spec/policies/transformation")
            .and_then(Value::as_object)
            .is_none_or(|v| v.len() != 1 || !v.contains_key("response"))
        || backend
            .pointer("/spec/policies/transformation/response")
            .and_then(Value::as_object)
            .is_none_or(|v| v.len() != 1 || !v.contains_key("set"))
    {
        return Err(invalid("ambiguous agentic backend identity policy"));
    }
    let set = array(backend, "/spec/policies/transformation/response/set")?;
    if set.len() != 1 || !string(&set[0], "/name")?.eq_ignore_ascii_case("x-thalamus-backend") {
        return Err(invalid(
            "agentic backend does not assert one revision header",
        ));
    }
    let value: String = serde_json::from_str(string(&set[0], "/value")?)
        .map_err(|_| invalid("agentic backend identity must be a literal"))?;
    if value != target.backend
        || value != format!("{service}/{}/{}", target.deployment, target.revision)
    {
        return Err(invalid(
            "agentic backend identity differs from model backend",
        ));
    }
    Ok(())
}

fn env_map(container: &Value) -> Result<BTreeMap<String, &Value>> {
    if container
        .get("envFrom")
        .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
    {
        return Err(invalid("agentic container has unresolved envFrom"));
    }
    let mut result = BTreeMap::new();
    for entry in array(container, "/env")? {
        let name = string(entry, "/name")?;
        if result.insert(name.to_owned(), entry).is_some() {
            return Err(invalid("duplicate agentic environment variable"));
        }
    }
    Ok(result)
}

fn env_literal<'a>(env: &'a BTreeMap<String, &Value>, key: &str) -> Result<&'a str> {
    let entry = env
        .get(key)
        .ok_or_else(|| invalid(format!("missing agentic {key}")))?;
    if entry.get("valueFrom").is_some() {
        return Err(invalid(
            "agentic routing environment is dynamically resolved",
        ));
    }
    string(entry, "/value")
}

fn secret_dependency(r: &LiveResources, ns: &str, entry: &Value, pod: &Value) -> Result<Value> {
    let reference = entry
        .pointer("/valueFrom/secretKeyRef")
        .ok_or_else(|| invalid("agentic secret reference missing"))?;
    if entry.get("value").is_some()
        || entry
            .pointer("/valueFrom")
            .and_then(Value::as_object)
            .is_none_or(|v| v.len() != 1)
        || reference
            .get("optional")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(invalid("unresolved agentic secret"));
    }
    let secret_name = string(reference, "/name")?;
    let key = string(reference, "/key")?;
    if !dns_host(secret_name) {
        return Err(invalid("invalid agentic secret name"));
    }
    let metadata = lookup(&r.agentic_secret_metadata, ns, secret_name)?;
    alive(metadata)?;
    let created = string(metadata, "/metadata/creationTimestamp")?;
    let pod_created = string(pod, "/metadata/creationTimestamp")?;
    let secret_time = DateTime::parse_from_rfc3339(created)
        .map_err(|_| invalid("invalid Secret creation time"))?;
    let pod_time = DateTime::parse_from_rfc3339(pod_created)
        .map_err(|_| invalid("invalid agentic Pod creation time"))?;
    if metadata.get("immutable").and_then(Value::as_bool) != Some(true) || secret_time >= pod_time {
        return Err(invalid(
            "agentic environment Secret is mutable or not older than its Pod",
        ));
    }
    Ok(
        json!({"name": secret_name, "key": key, "uid": uid(metadata)?, "resourceVersion": string(metadata, "/metadata/resourceVersion")?, "creationTimestamp": created}),
    )
}

fn http_probe(container: &Value, field: &str, path: &str) -> Result<()> {
    let probe = container
        .get(field)
        .ok_or_else(|| invalid("agentic HTTP probe missing"))?;
    let request = probe
        .get("httpGet")
        .ok_or_else(|| invalid("agentic probe is not HTTP"))?;
    if probe.get("exec").is_some()
        || probe.get("tcpSocket").is_some()
        || probe.get("grpc").is_some()
        || request.get("path").and_then(Value::as_str) != Some(path)
        || request.get("port").and_then(Value::as_u64) != Some(9000)
        || request
            .get("host")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        || request
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or("HTTP")
            != "HTTP"
        || request
            .get("httpHeaders")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid("unsafe or overridden agentic HTTP probe"));
    }
    Ok(())
}

fn agentic_pod_configuration(
    r: &LiveResources,
    config: &RuntimeConfig,
    deployment: &Value,
    pods: &[&Value],
    expected_base: &str,
    expected_mcp_hosts: &BTreeSet<String>,
) -> Result<(Value, Value, String, Vec<String>)> {
    rollout(deployment, true)?;
    let template = deployment
        .pointer("/spec/template/spec")
        .ok_or_else(|| invalid("agentic Pod template missing"))?;
    let template_containers = array(template, "/containers")?;
    if template_containers.len() != 1 {
        return Err(invalid("agentic Deployment has extra containers"));
    }
    let template_container = &template_containers[0];
    if string(template_container, "/name")? != "agentic-api" {
        return Err(invalid("unexpected agentic container"));
    }
    let image = string(template_container, "/image")?;
    let image_digest = pinned_image(image)?;
    if !config
        .trusted_agentic_images
        .iter()
        .any(|trusted| trusted == image)
    {
        return Err(invalid("agentic image is not independently trusted"));
    }
    let mut common: Option<Value> = None;
    let mut dependencies: Option<Value> = None;
    let mut pod_ids = Vec::new();
    for pod in pods {
        let spec = pod
            .get("spec")
            .ok_or_else(|| invalid("agentic Pod spec missing"))?;
        if spec
            .get("hostNetwork")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || spec
                .get("hostPID")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || spec
                .get("hostIPC")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || spec
                .get("hostAliases")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            || spec.get("dnsConfig").is_some()
            || spec
                .get("dnsPolicy")
                .and_then(Value::as_str)
                .unwrap_or("ClusterFirst")
                != "ClusterFirst"
            || spec
                .get("automountServiceAccountToken")
                .and_then(Value::as_bool)
                != Some(false)
            || spec
                .get("initContainers")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            || spec
                .get("ephemeralContainers")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
        {
            return Err(invalid("unsafe agentic Pod networking or sidecars"));
        }
        let containers = array(pod, "/spec/containers")?;
        if containers.len() != 1 || string(&containers[0], "/name")? != "agentic-api" {
            return Err(invalid("agentic Pod has extra or missing containers"));
        }
        let c = &containers[0];
        if c.as_object().is_none_or(|obj| {
            obj.keys().any(|key| {
                ![
                    "name",
                    "image",
                    "command",
                    "args",
                    "env",
                    "envFrom",
                    "resources",
                    "securityContext",
                    "volumeMounts",
                    "ports",
                    "startupProbe",
                    "readinessProbe",
                    "livenessProbe",
                    "imagePullPolicy",
                    "terminationMessagePath",
                    "terminationMessagePolicy",
                ]
                .contains(&key.as_str())
            })
        }) || string(c, "/image")? != image
            || c.get("command")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
            || c.get("args")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
            || c.pointer("/securityContext/privileged")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || c.pointer("/securityContext/allowPrivilegeEscalation")
                .and_then(Value::as_bool)
                != Some(false)
            || c.pointer("/securityContext/runAsNonRoot")
                .and_then(Value::as_bool)
                != Some(true)
            || c.pointer("/securityContext/seccompProfile/type")
                .and_then(Value::as_str)
                != Some("RuntimeDefault")
            || c.pointer("/securityContext/capabilities/drop")
                .and_then(Value::as_array)
                .is_none_or(|v| !v.iter().any(|x| x.as_str() == Some("ALL")))
            || c.pointer("/securityContext/capabilities/add")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            || c.pointer("/securityContext/runAsUser")
                .and_then(Value::as_i64)
                == Some(0)
            || c.pointer("/securityContext/procMount")
                .and_then(Value::as_str)
                .is_some_and(|v| v != "Default")
        {
            return Err(invalid("unsafe or overridden agentic container"));
        }
        http_probe(c, "startupProbe", "/health")?;
        http_probe(c, "livenessProbe", "/health")?;
        http_probe(c, "readinessProbe", "/ready")?;
        for field in [
            "image",
            "command",
            "args",
            "env",
            "envFrom",
            "ports",
            "volumeMounts",
            "securityContext",
            "resources",
            "readinessProbe",
            "livenessProbe",
            "startupProbe",
        ] {
            if template_container.get(field).is_some()
                && template_container.get(field) != c.get(field)
            {
                return Err(invalid(format!(
                    "agentic Pod {field} differs from deployment template"
                )));
            }
        }
        let ids = image_ids(pod, true)?;
        if ids.len() != 1 || ids.get("container/agentic-api") != Some(&image_digest) {
            return Err(invalid(
                "agentic actual imageID differs from trusted immutable image",
            ));
        }
        let env = env_map(c)?;
        if env_literal(&env, "LLM_API_BASE")? != expected_base
            || env_literal(&env, "SSL_CERT_FILE")? != CA_FILE
        {
            return Err(invalid("agentic inference base or trust path changed"));
        }
        if env_literal(&env, "SKIP_LLM_READY_CHECK")? != "true" {
            return Err(invalid("agentic readiness configuration differs"));
        }
        let mcp = env
            .get("AGENTIC_MCP_ALLOWED_HOSTS")
            .ok_or_else(|| invalid("agentic MCP host configuration missing"))?;
        let mcp_value = mcp.get("value").and_then(Value::as_str).unwrap_or("");
        let pod_hosts: BTreeSet<_> = if mcp_value.is_empty() {
            BTreeSet::new()
        } else {
            mcp_value.split(',').map(str::to_owned).collect()
        };
        if &pod_hosts != expected_mcp_hosts {
            return Err(invalid(
                "agentic Pod MCP hosts differ from the reviewed route guard",
            ));
        }
        let allowed = [
            "LLM_API_BASE",
            "SSL_CERT_FILE",
            "DATABASE_URL",
            "SKIP_LLM_READY_CHECK",
            "AGENTIC_MCP_ALLOWED_HOSTS",
            "AGENTIC_WEB_SEARCH_PROVIDER",
            "AGENTIC_WEB_SEARCH_BASE_URL",
            "YOU_API_KEY",
            "BRAVE_API_KEY",
            "TAVILY_API_KEY",
            "SERPLY_API_KEY",
            "SEARXNG_API_KEY",
        ];
        if env.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid("unreviewed agentic environment variable"));
        }
        let mut secrets = BTreeMap::new();
        for (key, entry) in &env {
            if *key == "DATABASE_URL" || key.ends_with("_API_KEY") {
                let dependency = secret_dependency(r, &config.namespace, entry, pod)?;
                if key == "DATABASE_URL"
                    && dependency.get("key").and_then(Value::as_str) != Some("DATABASE_URL")
                {
                    return Err(invalid("agentic database secret key changed"));
                }
                secrets.insert(key.clone(), dependency);
            } else {
                if entry.get("valueFrom").is_some()
                    || entry
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .contains("$(")
                {
                    return Err(invalid("unresolved agentic environment"));
                }
            }
        }
        if !secrets.contains_key("DATABASE_URL") {
            return Err(invalid("agentic database secret missing"));
        }
        let mounts = array(c, "/volumeMounts")?;
        let volumes = array(pod, "/spec/volumes")?;
        if mounts.len() != 1
            || volumes.len() != 1
            || mounts[0].get("mountPath").and_then(Value::as_str) != Some(CA_PATH)
            || mounts[0].get("readOnly").and_then(Value::as_bool) != Some(true)
            || mounts[0].get("subPath").is_some()
            || mounts[0].get("subPathExpr").is_some()
            || mounts[0]
                .get("mountPropagation")
                .and_then(Value::as_str)
                .is_some_and(|v| v != "None")
            || mounts[0].get("name") != volumes[0].get("name")
            || volumes[0]
                .as_object()
                .is_none_or(|v| v.keys().any(|k| k != "name" && k != "configMap"))
        {
            return Err(invalid("agentic Pod has an extra or unsafe volume mount"));
        }
        let cm_name = string(&volumes[0], "/configMap/name")?;
        if !cm_name.starts_with("thalamus-") || !dns_host(cm_name) {
            return Err(invalid(
                "agentic CA ConfigMap is not in the administrator-reserved namespace",
            ));
        }
        if volumes[0]
            .pointer("/configMap/optional")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(invalid("agentic CA ConfigMap is optional"));
        }
        if let Some(items) = volumes[0]
            .pointer("/configMap/items")
            .and_then(Value::as_array)
            && (items.len() != 1
                || items[0].get("key").and_then(Value::as_str) != Some("ca.crt")
                || items[0].get("path").and_then(Value::as_str) != Some("ca.crt"))
        {
            return Err(invalid("agentic CA ConfigMap projection differs"));
        }
        let ca = lookup(&r.config_maps, &config.namespace, cm_name)?;
        alive(ca)?;
        let pem = string(ca, "/data/ca.crt")?;
        let ca_created = string(ca, "/metadata/creationTimestamp")?;
        let ca_time = DateTime::parse_from_rfc3339(ca_created)
            .map_err(|_| invalid("invalid CA ConfigMap creation time"))?;
        let pod_time = DateTime::parse_from_rfc3339(string(pod, "/metadata/creationTimestamp")?)
            .map_err(|_| invalid("invalid agentic Pod creation time"))?;
        if ca.get("immutable").and_then(Value::as_bool) != Some(true)
            || ca_time >= pod_time
            || !pem.contains("-----BEGIN CERTIFICATE-----")
            || !pem.contains("-----END CERTIFICATE-----")
        {
            return Err(invalid(
                "agentic CA bundle is missing, mutable or newer than the Pod",
            ));
        }
        let effective = normalized_pod(pod)?;
        let deps = json!({"ca": {"name": cm_name, "uid": uid(ca)?, "creationTimestamp": ca_created, "data": ca.get("data"), "binaryData": ca.get("binaryData")}, "secrets": secrets});
        if common
            .as_ref()
            .is_some_and(|previous| previous != &effective)
            || dependencies
                .as_ref()
                .is_some_and(|previous| previous != &deps)
        {
            return Err(invalid("agentic selected Pods have mixed configuration"));
        }
        common = Some(effective);
        dependencies = Some(deps);
        pod_ids.push(uid(pod)?.to_owned());
    }
    let expected_replicas = deployment
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if pods.len() as u64 != expected_replicas {
        return Err(invalid(
            "agentic Ready endpoints differ from completed rollout",
        ));
    }
    Ok((
        common.ok_or_else(|| invalid("agentic Pod configuration missing"))?,
        dependencies.ok_or_else(|| invalid("agentic dependencies missing"))?,
        image_digest,
        pod_ids,
    ))
}

fn resolved_target_port(port: &Value, pod: &Value) -> Result<u64> {
    let target = port
        .get("targetPort")
        .or_else(|| port.get("port"))
        .ok_or_else(|| invalid("Service target port missing"))?;
    if let Some(number) = target.as_u64().filter(|n| *n > 0 && *n <= 65535) {
        return Ok(number);
    }
    let wanted = target
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("invalid Service target port"))?;
    let mut found = None;
    for container in array(pod, "/spec/containers")? {
        for item in container
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("name").and_then(Value::as_str) == Some(wanted)
                && item
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("TCP")
                    == "TCP"
            {
                let number = item
                    .get("containerPort")
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0 && *n <= 65535)
                    .ok_or_else(|| invalid("invalid named Pod port"))?;
                if found.replace(number).is_some() {
                    return Err(invalid("ambiguous named Pod port"));
                }
            }
        }
    }
    found.ok_or_else(|| invalid("named Service target port not found"))
}

/// A selector does not rule out foreign, manually published EndpointSlices.
/// Bind every routable endpoint to the selected Pod UID and its observed IP.
fn endpoint_evidence(
    r: &LiveResources,
    service: &Value,
    service_port: &Value,
    labels: &BTreeMap<String, String>,
    ready_pods: &[&Value],
) -> Result<Value> {
    let ns = namespace(service)?;
    let service_name = name(service)?;
    let mut expected = BTreeMap::new();
    for pod in r
        .pods
        .iter()
        .filter(|p| namespace(p).ok() == Some(ns) && selected(p, labels))
    {
        if expected.insert(uid(pod)?.to_owned(), pod).is_some() {
            return Err(invalid("duplicate selected Pod UID"));
        }
    }
    let ready: BTreeSet<_> = ready_pods
        .iter()
        .map(|p| uid(p).map(str::to_owned))
        .collect::<Result<_>>()?;
    let mut reached = BTreeSet::new();
    let mut addresses_seen = BTreeSet::new();
    let mut evidence = Vec::new();
    let mut slice_ids = BTreeSet::new();
    for slice in r.agentic_endpoint_slices.iter().filter(|s| {
        namespace(s).ok() == Some(ns)
            && s.pointer("/metadata/labels/kubernetes.io~1service-name")
                .and_then(Value::as_str)
                == Some(service_name)
    }) {
        alive(slice)?;
        if !slice_ids.insert(uid(slice)?.to_owned())
            || slice
                .pointer("/metadata/labels/endpointslice.kubernetes.io~1managed-by")
                .and_then(Value::as_str)
                != Some("endpointslice-controller.k8s.io")
        {
            return Err(invalid("foreign or duplicate EndpointSlice"));
        }
        let owner = crate::observe::controller_owner(slice)?;
        if string(owner, "/kind")? != "Service"
            || string(owner, "/name")? != service_name
            || string(owner, "/uid")? != uid(service)?
        {
            return Err(invalid(
                "EndpointSlice does not belong to the observed Service",
            ));
        }
        let family = string(slice, "/addressType")?;
        if !matches!(family, "IPv4" | "IPv6") {
            return Err(invalid("unsupported EndpointSlice address family"));
        }
        let mut matching_ports = array(slice, "/ports")?.iter().filter(|p| {
            p.get("name").and_then(Value::as_str)
                == service_port.get("name").and_then(Value::as_str)
                && p.get("protocol").and_then(Value::as_str).unwrap_or("TCP") == "TCP"
        });
        let endpoint_port = matching_ports
            .next()
            .and_then(|p| p.get("port"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0 && *n <= 65535)
            .ok_or_else(|| invalid("EndpointSlice port is missing"))?;
        if matching_ports.next().is_some() {
            return Err(invalid("EndpointSlice port is ambiguous"));
        }
        for endpoint in array(slice, "/endpoints")? {
            let reference = endpoint
                .get("targetRef")
                .ok_or_else(|| invalid("EndpointSlice targetRef missing"))?;
            let pod_uid = string(reference, "/uid")?;
            let pod = expected
                .get(pod_uid)
                .ok_or_else(|| invalid("EndpointSlice contains foreign Pod"))?;
            if string(reference, "/kind")? != "Pod"
                || string(reference, "/name")? != name(pod)?
                || reference
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or(ns)
                    != ns
                || endpoint_port != resolved_target_port(service_port, pod)?
            {
                return Err(invalid("EndpointSlice target or port mismatch"));
            }
            let conditions = endpoint
                .get("conditions")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid("EndpointSlice conditions missing"))?;
            let is_ready = conditions
                .get("ready")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("EndpointSlice Ready is unknown"))?;
            let terminating = conditions
                .get("terminating")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let serving = conditions
                .get("serving")
                .and_then(Value::as_bool)
                .unwrap_or(is_ready);
            if (is_ready && (!ready.contains(pod_uid) || terminating || !serving))
                || (!is_ready && serving)
            {
                return Err(invalid(
                    "EndpointSlice publishes an unverified or terminating endpoint",
                ));
            }
            let mut pod_ips = BTreeSet::new();
            if let Some(ip) = pod.pointer("/status/podIP").and_then(Value::as_str) {
                pod_ips.insert(ip);
            }
            for ip in pod
                .pointer("/status/podIPs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                pod_ips.insert(string(ip, "/ip")?);
            }
            let addresses = array(endpoint, "/addresses")?;
            if addresses.is_empty() {
                return Err(invalid("EndpointSlice address missing"));
            }
            for addr in addresses {
                let addr = addr
                    .as_str()
                    .ok_or_else(|| invalid("invalid EndpointSlice address"))?;
                let parsed: std::net::IpAddr = addr
                    .parse()
                    .map_err(|_| invalid("invalid EndpointSlice IP"))?;
                if (family == "IPv4") != parsed.is_ipv4()
                    || !pod_ips.contains(addr)
                    || !addresses_seen.insert((family.to_owned(), addr.to_owned(), endpoint_port))
                {
                    return Err(invalid("EndpointSlice address does not identify one Pod"));
                }
            }
            if is_ready {
                reached.insert(pod_uid.to_owned());
            }
        }
        evidence.push(json!({"uid":uid(slice)?, "addressType":family, "ports":slice.get("ports"), "endpoints":slice.get("endpoints")}));
    }
    if evidence.is_empty() || reached != ready {
        return Err(invalid(
            "EndpointSlices do not contain exactly the Ready selected Pods",
        ));
    }
    evidence.sort_by(|a, b| {
        a.pointer("/uid")
            .and_then(Value::as_str)
            .cmp(&b.pointer("/uid").and_then(Value::as_str))
    });
    Ok(Value::Array(evidence))
}

fn gateway_service<'a>(
    r: &'a LiveResources,
    config: &RuntimeConfig,
    route: &Value,
    hosts: &[String],
) -> Result<(&'a Value, &'a Value, Value)> {
    let parents = array(route, "/spec/parentRefs")?;
    if parents.len() != 1 {
        return Err(invalid("internal agentic route requires one Gateway"));
    }
    let (ns, gateway_name) = parent_key(&parents[0], namespace(route)?)?;
    if ns != config.namespace
        || !dns_label(&gateway_name)
        || hosts != [format!("{gateway_name}.{ns}.svc.cluster.local")]
    {
        return Err(invalid(
            "internal agentic hostname or Gateway namespace mismatch",
        ));
    }
    let gateway = lookup(&r.gateways, &ns, &gateway_name)?;
    let service = lookup(&r.agentic_services, &ns, &gateway_name)?;
    let labels = service_shape(service)?;
    if labels.get("gateway.networking.k8s.io/gateway-name") != Some(&gateway_name) {
        return Err(invalid("internal Service does not select its Gateway"));
    }
    let service_port = port(service, 443)?;
    let pods = serving_pods(r, &ns, &labels, gateway)?;
    // Agentgateway v1.7 binds Gateway listeners dynamically; its Pod template
    // need not declare a containerPort for a numeric listener targetPort.
    // Kubernetes named targetPorts, however, must resolve through a declared
    // Pod port. In both cases the endpoint must match the HTTPS listener.
    for pod in &pods {
        if resolved_target_port(service_port, pod)? != 443 {
            return Err(invalid(
                "internal Gateway Service target does not match the HTTPS listener",
            ));
        }
    }
    let endpoints = endpoint_evidence(r, service, service_port, &labels, &pods)?;
    Ok((gateway, service, endpoints))
}

/// Join a separately observed direct route to the actual agentic route.
/// Nothing supplied by the HTTP caller or asserted solely in a response header
/// can stand in for any of the live hops.
pub fn join_agentic_route(
    config: &RuntimeConfig,
    service: &str,
    route_name: &str,
    direct: &RouteSnapshot,
    r: &LiveResources,
    observed_at: DateTime<Utc>,
) -> Result<RouteSnapshot> {
    let service_config = config
        .routes
        .get(service)
        .ok_or_else(|| invalid("unknown agentic service"))?;
    if service_config.service.name != service
        || service_config.team_id.is_nil()
        || service_config.project_id.is_nil()
        || !dns_label(service)
        || direct.service != service
        || direct.agentic
    {
        return Err(invalid("invalid direct/agentic service identity"));
    }
    let freshly_direct = join_route(config, service, &direct.route, r, observed_at)?;
    if !direct.same_identity(&freshly_direct) {
        return Err(invalid("direct route changed during agentic observation"));
    }
    let route = lookup(&r.routes, &config.namespace, route_name)?;
    alive(route)?;
    generation(route)?;
    let instance = annotation(route, "thalamus.ai/agentic-instance")?;
    let team = annotation(route, "thalamus.ai/team")?;
    let role = annotation(route, "thalamus.ai/agentic-role")?;
    let team_id = service_config.team_id.to_string();
    if !dns_label(instance) || (team != "shared" && team != team_id) {
        return Err(invalid(
            "agentic instance or team does not match the service",
        ));
    }
    resource_annotations(route, service, Some(instance), Some(team), None)?;
    let candidate = match role {
        "public" => {
            if route_name != format!("{service}-{instance}")
                || direct.route != service_config.public
            {
                return Err(invalid(
                    "agentic public route does not bind the direct public route",
                ));
            }
            false
        }
        "candidate" => {
            if direct.deployments.len() != 1 || direct.weights.len() != 1 {
                return Err(invalid("agentic candidate requires one direct deployment"));
            }
            let target = &direct.deployments[0];
            if direct.route != format!("{}-candidate", target.deployment)
                || route_name
                    != format!(
                        "agentic-cand-{}",
                        hash_name(service, instance, &target.deployment)
                    )
                || direct
                    .weights
                    .get(&target.deployment)
                    .is_none_or(|weight| *weight == 0)
            {
                return Err(invalid(
                    "agentic candidate route is not bound to its direct deployment",
                ));
            }
            resource_annotations(route, service, Some(instance), Some(team), Some(target))?;
            true
        }
        _ => return Err(invalid("unknown agentic route role")),
    };
    // Discovery must not silently choose between a team instance and shared
    // instance, even if one happens to be listed first by the API server.
    let applicable: Vec<_> = r
        .routes
        .iter()
        .filter(|v| {
            namespace(v).ok() == Some(config.namespace.as_str())
                && annotation(v, "thalamus.ai/service").ok() == Some(service)
                && annotation(v, "thalamus.ai/agentic-role").ok() == Some("public")
                && annotation(v, "thalamus.ai/team")
                    .ok()
                    .is_some_and(|v| v == "shared" || v == team_id)
        })
        .collect();
    if applicable.len() != 1
        || annotation(applicable[0], "thalamus.ai/agentic-instance")? != instance
        || annotation(applicable[0], "thalamus.ai/team")? != team
        || name(applicable[0])? != format!("{service}-{instance}")
    {
        return Err(invalid(
            "agentic service has missing or ambiguous applicable public instances",
        ));
    }
    let public_hosts = route_hosts(applicable[0])?;
    let hosts = route_hosts(route)?;
    if hosts != public_hosts {
        return Err(invalid("candidate and public agentic hostnames differ"));
    }
    let candidate_target = candidate.then(|| &direct.deployments[0]);
    if candidate {
        if route.pointer("/spec/parentRefs") != applicable[0].pointer("/spec/parentRefs") {
            return Err(invalid("candidate and public agentic Gateways differ"));
        }
        let target = &direct.deployments[0];
        let candidates = r
            .routes
            .iter()
            .filter(|v| {
                namespace(v).ok() == Some(config.namespace.as_str())
                    && annotation(v, "thalamus.ai/service").ok() == Some(service)
                    && annotation(v, "thalamus.ai/agentic-role").ok() == Some("candidate")
                    && annotation(v, "thalamus.ai/agentic-instance").ok() == Some(instance)
                    && annotation(v, "thalamus.ai/deployment").ok()
                        == Some(target.deployment.as_str())
            })
            .count();
        if candidates != 1 {
            return Err(invalid("agentic candidate discovery is ambiguous"));
        }
    }
    let mut expected_refs = BTreeMap::new();
    let mut targets = BTreeMap::new();
    for target in &direct.deployments {
        if !dns_label(&target.deployment)
            || targets.insert(target.deployment.clone(), target).is_some()
        {
            return Err(invalid("duplicate or invalid direct deployment"));
        }
        let weight = *direct
            .weights
            .get(&target.deployment)
            .ok_or_else(|| invalid("direct deployment weight missing"))?;
        let backend = format!(
            "agentic-backend-{}",
            hash_name(service, instance, &target.deployment)
        );
        if expected_refs.insert(backend, weight).is_some() {
            return Err(invalid("agentic backend name collision"));
        }
    }
    if targets.is_empty() || targets.len() != direct.weights.len() {
        return Err(invalid(
            "direct route weights and deployments are ambiguous",
        ));
    }
    let prefix = candidate_target.map(|target| format!("/candidates/{}", target.deployment));
    let expected_matches = if let Some(prefix) = &prefix {
        CANDIDATE_PATHS
            .iter()
            .map(|path| ("POST".into(), "Exact".into(), format!("{prefix}{path}")))
            .collect()
    } else {
        expected_public(team != "shared")
    };
    let paths = route_matches(
        route,
        &expected_matches,
        prefix.as_deref(),
        &expected_refs,
        "agentgateway.dev",
        "AgentgatewayBackend",
    )?;
    let listeners = tls_listeners(r, route, &hosts)?;
    let ports = listeners
        .iter()
        .map(|l| l.port)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    competing(r, route, &hosts, &expected_matches)?;
    check_route_policy(
        r,
        route,
        service,
        RoutePolicyMode::AgenticOuter {
            candidate: candidate_target.map(|v| v.deployment.as_str()),
        },
        service_config.team_id,
    )?;
    let outer_policy = route_policy(r, route)?;
    let mcp_hosts = check_outer_guards(
        outer_policy,
        team,
        candidate_target.map(|t| t.deployment.as_str()),
    )?;
    let service_suffixes = [
        format!(".{}", config.namespace),
        format!(".{}.svc", config.namespace),
        format!(".{}.svc.cluster.local", config.namespace),
    ];
    for host in &mcp_hosts {
        if hosts.contains(host) {
            return Err(invalid("MCP origin aliases the public agentic route"));
        }
        if host.starts_with("agentic-pod-")
            && service_suffixes
                .iter()
                .any(|suffix| host.ends_with(suffix.as_str()))
        {
            return Err(invalid("MCP origin aliases an agentic Pod Service"));
        }
    }
    let (outer_images, outer_gateway_config, outer_gateway_uids) = gateway_evidence(r, route)?;
    let direct_route = lookup(&r.routes, &config.namespace, &direct.route)?;
    let mut deployments = Vec::with_capacity(direct.deployments.len());
    for original in &direct.deployments {
        let hash = hash_name(service, instance, &original.deployment);
        let pod_name = format!("agentic-pod-{hash}");
        let backend_name = format!("agentic-backend-{hash}");
        let backend = lookup(&r.backends, &config.namespace, &backend_name)?;
        let pod_service = lookup(&r.agentic_services, &config.namespace, &pod_name)?;
        let deployment = lookup(&r.agentic_deployments, &config.namespace, &pod_name)?;
        for resource in [backend, pod_service, deployment] {
            resource_annotations(
                resource,
                service,
                Some(instance),
                Some(team),
                Some(original),
            )?;
        }
        if string(deployment, "/kind")? != "Deployment" {
            return Err(invalid("agentic workload is not a Deployment"));
        }
        let backend_host = format!("{pod_name}.{}.svc.cluster.local", config.namespace);
        static_backend(backend, &backend_host, original, service)?;
        let labels = service_shape(pod_service)?;
        if labels.len() != 1
            || labels.get("thalamus.dev/agentic-instance") != Some(&pod_name)
            || deployment
                .pointer("/spec/selector/matchExpressions")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            || deployment.pointer("/spec/selector/matchLabels")
                != pod_service.pointer("/spec/selector")
            || !selected(
                deployment
                    .pointer("/spec/template")
                    .ok_or_else(|| invalid("agentic Deployment template missing"))?,
                &labels,
            )
            || array(pod_service, "/spec/ports")?.len() != 1
        {
            return Err(invalid("agentic Deployment and Service selectors disagree"));
        }
        let service_port = port(pod_service, 9000)?;
        let pods = serving_pods(r, &config.namespace, &labels, deployment)?;
        for pod in &pods {
            target_port_matches(service_port, pod)?;
        }
        let pod_endpoints = endpoint_evidence(r, pod_service, service_port, &labels, &pods)?;
        let internal_name = format!("{}-agentic-internal", original.deployment);
        let internal = lookup(&r.routes, &config.namespace, &internal_name)?;
        resource_annotations(internal, service, None, None, Some(original))?;
        generation(internal)?;
        let internal_hosts = route_hosts(internal)?;
        let internal_prefix = format!("/agentic/{service}/{}", original.deployment);
        let internal_matches: BTreeSet<_> = CANDIDATE_PATHS
            .iter()
            .map(|path| {
                (
                    "POST".into(),
                    "Exact".into(),
                    format!("{internal_prefix}{path}"),
                )
            })
            .collect();
        let model_backend = format!("{}-backend", original.deployment);
        let model_refs = BTreeMap::from([(model_backend.clone(), 1)]);
        route_matches(
            internal,
            &internal_matches,
            Some(&internal_prefix),
            &model_refs,
            "agentgateway.dev",
            "AgentgatewayBackend",
        )?;
        let internal_listeners = tls_listeners(r, internal, &internal_hosts)?;
        if internal_listeners.iter().any(|l| l.port != 443) {
            return Err(invalid("internal agentic Gateway is not HTTPS on port 443"));
        }
        competing(r, internal, &internal_hosts, &internal_matches)?;
        check_route_policy(
            r,
            internal,
            service,
            RoutePolicyMode::AgenticInternal {
                deployment: &original.deployment,
                candidate,
            },
            service_config.team_id,
        )?;
        let internal_policy = route_policy(r, internal)?;
        let normalized_internal_policy =
            normalized_scope(internal_policy, &original.deployment, candidate)?;
        let models_name = format!("{}-agentic-models", original.deployment);
        let models_route = lookup(&r.routes, &config.namespace, &models_name)?;
        resource_annotations(models_route, service, None, None, Some(original))?;
        generation(models_route)?;
        if models_route.pointer("/spec/parentRefs") != internal.pointer("/spec/parentRefs")
            || route_hosts(models_route)? != internal_hosts
        {
            return Err(invalid(
                "agentic models route uses a different private Gateway",
            ));
        }
        let ai_backend = lookup(&r.backends, &config.namespace, &model_backend)?;
        let pool_name = string(ai_backend, "/spec/ai/provider/custom/backendRef/name")?;
        if pool_name != format!("{}-inference-pool", original.deployment) {
            return Err(invalid(
                "agentic models route pool differs from direct AI backend",
            ));
        }
        let model_matches = BTreeSet::from([(
            "GET".to_owned(),
            "Exact".to_owned(),
            format!("{internal_prefix}/v1/models"),
        )]);
        let pool_refs = BTreeMap::from([(pool_name.to_owned(), 1)]);
        route_matches(
            models_route,
            &model_matches,
            Some(&internal_prefix),
            &pool_refs,
            "inference.networking.k8s.io",
            "InferencePool",
        )?;
        let models_listeners = tls_listeners(r, models_route, &internal_hosts)?;
        if models_listeners != internal_listeners {
            return Err(invalid(
                "agentic models route listener differs from private inference",
            ));
        }
        competing(r, models_route, &internal_hosts, &model_matches)?;
        check_route_policy(
            r,
            models_route,
            service,
            RoutePolicyMode::AgenticModels {
                deployment: &original.deployment,
                candidate,
            },
            service_config.team_id,
        )?;
        let models_policy = route_policy(r, models_route)?;
        let normalized_models_policy =
            normalized_scope(models_policy, &original.deployment, candidate)?;
        let (gateway, gateway_svc, gateway_endpoints) =
            gateway_service(r, config, internal, &internal_hosts)?;
        let (internal_images, internal_gateway_config, internal_gateway_uids) =
            gateway_evidence(r, internal)?;
        let (gateway_ns, gateway_name) =
            parent_key(&array(internal, "/spec/parentRefs")?[0], &config.namespace)?;
        for host in [
            format!("{gateway_name}.{gateway_ns}"),
            format!("{gateway_name}.{gateway_ns}.svc"),
            format!("{gateway_name}.{gateway_ns}.svc.cluster.local"),
            format!("{pod_name}.{}", config.namespace),
            format!("{pod_name}.{}.svc", config.namespace),
            format!("{pod_name}.{}.svc.cluster.local", config.namespace),
        ] {
            if mcp_hosts.contains(&host) {
                return Err(invalid(
                    "MCP origin aliases an agentic or internal Gateway Service",
                ));
            }
        }
        for parent in array(route, "/spec/parentRefs")? {
            if parent_key(parent, &config.namespace)? == (gateway_ns.clone(), gateway_name.clone())
            {
                return Err(invalid(
                    "public and internal agentic Gateways are not isolated",
                ));
            }
        }
        let base =
            format!("https://{gateway_name}.{gateway_ns}.svc.cluster.local{internal_prefix}");
        let (effective, deps, image, pod_uids) =
            agentic_pod_configuration(r, config, deployment, &pods, &base, &mcp_hosts)?;
        // This walks the real AI backend, inference pool, picker, serving
        // workloads and Ready model Pods again; a header is never proof.
        let rejoined = join_backend(config, service, &model_backend, direct_route, r)?;
        if &rejoined != original {
            return Err(invalid(
                "internal agentic backend differs from direct model chain",
            ));
        }
        let mut target = original.clone();
        target.underlying_model = Some(UnderlyingModelIdentity {
            rollout_configuration: original.rollout_configuration.clone(),
            runtime: original.runtime.clone(),
        });
        let outer_identity = json!({"route":route.get("spec"), "policy":outer_policy.get("spec"), "gateways":outer_gateway_config});
        let inner_identity = json!({
            "route":internal.get("spec"), "policy":internal_policy.get("spec"),
            "models_route":models_route.get("spec"), "models_policy":models_policy.get("spec"),
            "gateway":gateway.get("spec"), "gateway_service":gateway_svc.get("spec"),
            "static_backend":backend.get("spec"), "service":pod_service.get("spec"),
            "deployment":deployment.get("spec")
        });
        let dependencies = json!({
            "configuration":deps, "agentic_pods":pod_uids,
            "agentic_endpoints":pod_endpoints, "gateway_endpoints":gateway_endpoints,
            "outer_gateways":outer_gateway_uids, "internal_gateways":internal_gateway_uids
        });
        target.desired.rendered_resources = canonical_digest(&json!({
            "direct":original.desired.rendered_resources, "outer":outer_identity, "agentic":inner_identity
        }))?;
        target.desired.resolved_dependencies = canonical_digest(&json!({
            "direct":original.desired.resolved_dependencies, "agentic":dependencies
        }))?;
        target.rollout_configuration = canonical_digest(&json!({
            "direct":original.rollout_configuration, "internal_route":internal.get("spec"),
            "internal_policy":normalized_internal_policy, "models_route":models_route.get("spec"),
            "models_policy":normalized_models_policy, "pod":effective,
            "outer_gateway":outer_gateway_config, "internal_gateway":internal_gateway_config,
            "configuration":deps
        }))?;
        target.runtime.effective_configuration = canonical_digest(&json!({
            "direct":original.runtime.effective_configuration, "agentic_pod":effective,
            "configuration":deps, "outer_gateway":outer_gateway_config, "internal_gateway":internal_gateway_config
        }))?;
        if target
            .runtime
            .component_versions
            .insert(format!("agentic/{pod_name}/image"), image.clone())
            .is_some()
        {
            return Err(invalid("duplicate agentic runtime image identity"));
        }
        for (key, value) in outer_images.iter().chain(internal_images.iter()) {
            if let Some(existing) = target.runtime.component_versions.get(key) {
                if existing != value {
                    return Err(invalid("gateway image identity disagreement"));
                }
            } else {
                target
                    .runtime
                    .component_versions
                    .insert(key.clone(), value.clone());
            }
        }
        for pod_uid in &pod_uids {
            if target
                .runtime
                .component_versions
                .insert(format!("agentic-pod-uid/{pod_uid}"), image.clone())
                .is_some()
            {
                return Err(invalid("duplicate agentic Pod runtime identity"));
            }
        }
        deployments.push(target);
    }
    Ok(RouteSnapshot {
        observed_at,
        service: service.to_owned(),
        route: route_name.to_owned(),
        hostnames: hosts,
        paths,
        ports,
        listeners,
        weights: direct.weights.clone(),
        deployments,
        agentic: true,
    })
}
