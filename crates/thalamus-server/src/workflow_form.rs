//! Platform-admin form data from one freshly fetched GitOps revision.
//!
//! The configured schema is an operator-maintained, KCL-generated JSON Schema
//! for the renderer's Service, committed with the catalog. It is not a substitute
//! for the renderer: KCL `check` expressions still run during validation.
use std::{
    collections::HashSet,
    path::{Component, Path as FsPath},
    process::Command,
    sync::Arc,
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderName, StatusCode},
    routing::get,
    Extension, Json, Router,
};
use serde::Serialize;
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::{
    workflows::{self, GitOpsConfig},
    ApiError, AppState, RequestPrincipal,
};

fn unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
}

/// All form data describe the same immutable GitOps commit. `source` and
/// `artifacts` are committed YAML data expressed as JSON without added defaults.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ServiceForm {
    service: String,
    base_commit: String,
    source: Map<String, Value>,
    /// Artifact name to the exact committed artifact object, including its URI
    /// and any model/tokenizer content identities.
    artifacts: Map<String, Value>,
    schema: Map<String, Value>,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/changes/form/{service}", get(get_form))
}

#[utoipa::path(
    get,
    path = "/api/v1/changes/form/{service}",
    params(("service" = String, Path, description = "Selected service name")),
    responses(
        (status = 200, body = ServiceForm, description = "Committed service, artifact catalog and KCL-generated form schema"),
        (status = 400, description = "Invalid service name"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "Platform administrator required"),
        (status = 404, description = "Service not found at the target revision"),
        (status = 503, description = "GitOps, artifact catalog or service schema unavailable")
    )
)]
pub(crate) async fn get_form(
    State(state): State<Arc<AppState>>,
    Path(service): Path<String>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<([(HeaderName, &'static str); 1], Json<ServiceForm>), ApiError> {
    let guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let config = workflows::gitops(&state)?.clone();
    let form = tokio::task::spawn_blocking(move || load_form(&config, &service))
        .await
        .map_err(|_| unavailable("service form loader failed"))??;
    guard.commit().await?;
    // Every request resolves the remote branch; intermediaries must not return
    // a previously resolved commit for a later editing session.
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(form)))
}

fn load_form(config: &GitOpsConfig, service: &str) -> Result<ServiceForm, ApiError> {
    if !workflows::valid_name(service) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid service name",
        ));
    }
    let policy = workflows::policy(config, service)?;
    let schema_path = config
        .service_schema_path
        .as_deref()
        .ok_or_else(|| unavailable("KCL service form schema is not configured"))?;
    let repo = workflows::open_repo(config, &policy.target_branch)?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    let base_commit = repo
        .resolve_base(&policy.target_branch)
        .map_err(|_| unavailable("configured target branch is unavailable"))?;
    if !full_object_id(&base_commit) {
        return Err(unavailable(
            "configured target branch has an invalid commit",
        ));
    }

    // The service name resolves only to a direct, regular catalog/services
    // blob in this same tree; the browser cannot choose the file path.
    let file = workflows::catalog_service_file(config, &base_commit, service)?;
    let source = parse_service(
        &read_regular_blob(&config.repository_dir, &base_commit, &file)?,
        service,
    )?;
    let artifacts = parse_artifacts(&read_regular_blob(
        &config.repository_dir,
        &base_commit,
        FsPath::new("catalog/artifacts.yaml"),
    )?)?;
    let schema = parse_schema(&read_regular_blob(
        &config.repository_dir,
        &base_commit,
        schema_path,
    )?)?;
    Ok(ServiceForm {
        service: service.to_owned(),
        base_commit,
        source,
        artifacts,
        schema,
    })
}

fn full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Inspect the tree entry before reading its object ID. This rejects symlinks,
/// directories, missing paths, pathspec expansions, and Git replacement objects.
/// The path must be selected by trusted server configuration or policy.
pub(crate) fn read_regular_blob(
    repository: &FsPath,
    commit: &str,
    path: &FsPath,
) -> Result<Vec<u8>, ApiError> {
    let invalid = || unavailable("committed GitOps file is unavailable or invalid");
    if !full_object_id(commit)
        || path.as_os_str().is_empty()
        || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(invalid());
    }
    let path = path.to_str().ok_or_else(invalid)?;
    let tree = Command::new("git")
        .args(["--literal-pathspecs", "--git-dir"])
        .arg(repository)
        .args(["ls-tree", "--full-tree", "-r", "-z", commit, "--", path])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(|_| invalid())?;
    if !tree.status.success() {
        return Err(invalid());
    }
    let entry = tree.stdout.strip_suffix(&[0]).ok_or_else(invalid)?;
    if entry.contains(&0) {
        return Err(invalid());
    }
    let tab = entry.iter().position(|b| *b == b'\t').ok_or_else(invalid)?;
    if &entry[tab + 1..] != path.as_bytes() {
        return Err(invalid());
    }
    let metadata = std::str::from_utf8(&entry[..tab]).map_err(|_| invalid())?;
    let mut fields = metadata.split(' ');
    let mode = fields.next();
    let kind = fields.next();
    let object = fields.next().ok_or_else(invalid)?;
    if fields.next().is_some()
        || !matches!(mode, Some("100644" | "100755"))
        || kind != Some("blob")
        || !full_object_id(object)
    {
        return Err(invalid());
    }
    let blob = Command::new("git")
        .arg("--git-dir")
        .arg(repository)
        .args(["cat-file", "blob", object])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(|_| invalid())?;
    if !blob.status.success() {
        return Err(invalid());
    }
    Ok(blob.stdout)
}

// YAML tags, non-string mapping keys and non-finite numbers have no faithful
// JSON representation; reject them instead of silently changing source data.
fn yaml_is_json(value: &serde_yaml::Value) -> bool {
    use serde_yaml::Value as Yaml;
    match value {
        Yaml::Null | Yaml::Bool(_) | Yaml::String(_) => true,
        Yaml::Number(number) => number.as_f64().is_none_or(f64::is_finite),
        Yaml::Sequence(values) => values.iter().all(yaml_is_json),
        Yaml::Mapping(values) => values
            .iter()
            .all(|(key, value)| matches!(key, Yaml::String(_)) && yaml_is_json(value)),
        Yaml::Tagged(_) => false,
    }
}

fn parse_yaml_object(bytes: &[u8], message: &'static str) -> Result<Map<String, Value>, ApiError> {
    let invalid = || unavailable(message);
    let yaml: serde_yaml::Value = serde_yaml::from_slice(bytes).map_err(|_| invalid())?;
    if !yaml_is_json(&yaml) {
        return Err(invalid());
    }
    match serde_json::to_value(yaml).map_err(|_| invalid())? {
        Value::Object(object) => Ok(object),
        _ => Err(invalid()),
    }
}

fn parse_service(bytes: &[u8], service: &str) -> Result<Map<String, Value>, ApiError> {
    let source = parse_yaml_object(bytes, "committed service cannot be represented as JSON")?;
    if source.get("name").and_then(Value::as_str) != Some(service) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "committed service identity does not match the selected service",
        ));
    }
    Ok(source)
}

fn parse_artifacts(bytes: &[u8]) -> Result<Map<String, Value>, ApiError> {
    let message = "committed artifact catalog is invalid";
    let artifacts = parse_yaml_object(bytes, message)?;
    if artifacts.values().any(|artifact| {
        artifact
            .as_object()
            .and_then(|object| object.get("uri"))
            .and_then(Value::as_str)
            .is_none_or(|uri| uri.is_empty())
    }) {
        return Err(unavailable(message));
    }
    Ok(artifacts)
}

fn schema_reference<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    if !pointer.starts_with('/') {
        return None;
    }
    // The configured single-file document must not depend on a network fetch,
    // an external file, or an unresolved reference. KCL schema names use local
    // JSON Pointers after the operator roots the generated Service schema.
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' || (ch == '~' && !matches!(chars.next(), Some('0' | '1'))) {
            return None;
        }
    }
    root.pointer(pointer)
}

fn unique_string_array(value: &Value) -> bool {
    let Some(values) = value.as_array() else {
        return false;
    };
    let mut seen = HashSet::with_capacity(values.len());
    values
        .iter()
        .all(|value| value.as_str().is_some_and(|s| seen.insert(s)))
}

fn valid_type(value: &Value) -> bool {
    let valid_name = |value: &str| {
        matches!(
            value,
            "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
        )
    };
    match value {
        Value::String(value) => valid_name(value),
        Value::Array(values) if !values.is_empty() && unique_string_array(value) => values
            .iter()
            .all(|value| value.as_str().is_some_and(valid_name)),
        _ => false,
    }
}

/// Check the schema keywords used by KCL exports, including the complete local
/// reference graph. Other vocabulary/annotation keywords are retained verbatim.
/// This is structural admission, not validation of a proposed service against
/// KCL's checks; the normal change validation still performs the real render.
fn valid_schema(root: &Value) -> bool {
    if !root.is_object() {
        return false;
    }
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(value) = pending.pop() {
        if !seen.insert(value as *const Value) {
            continue;
        }
        let object = match value {
            Value::Bool(_) => continue,
            Value::Object(object) => object,
            _ => return false,
        };
        // Nested resource IDs and dynamic references have resolution semantics
        // beyond a single local document. Do not hand the UI a misleading form.
        if object.contains_key("$dynamicRef")
            || object.contains_key("$recursiveRef")
            || (!std::ptr::eq(value, root) && object.contains_key("$id"))
        {
            return false;
        }
        if object.get("type").is_some_and(|value| !valid_type(value)) {
            return false;
        }
        if let Some(reference) = object.get("$ref") {
            let Some(target) = reference.as_str().and_then(|s| schema_reference(root, s)) else {
                return false;
            };
            if !target.is_object() && !target.is_boolean() {
                return false;
            }
            pending.push(target);
        }
        for key in [
            "$schema",
            "$id",
            "$anchor",
            "$comment",
            "title",
            "description",
            "format",
            "pattern",
        ] {
            if object.get(key).is_some_and(|value| !value.is_string()) {
                return false;
            }
        }
        for key in ["readOnly", "writeOnly", "deprecated", "uniqueItems"] {
            if object.get(key).is_some_and(|value| !value.is_boolean()) {
                return false;
            }
        }
        for key in [
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "minProperties",
            "maxProperties",
            "minContains",
            "maxContains",
        ] {
            if object
                .get(key)
                .is_some_and(|value| value.as_u64().is_none())
            {
                return false;
            }
        }
        for key in ["minimum", "maximum"] {
            if object.get(key).is_some_and(|value| !value.is_number()) {
                return false;
            }
        }
        for key in ["exclusiveMinimum", "exclusiveMaximum"] {
            // Older JSON Schema drafts use booleans for these two keywords.
            if object
                .get(key)
                .is_some_and(|value| !value.is_number() && !value.is_boolean())
            {
                return false;
            }
        }
        if object
            .get("multipleOf")
            .is_some_and(|value| value.as_f64().is_none_or(|number| number <= 0.0))
            || object
                .get("required")
                .is_some_and(|value| !unique_string_array(value))
            || object
                .get("enum")
                .is_some_and(|value| value.as_array().is_none_or(Vec::is_empty))
        {
            return false;
        }
        for key in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
        ] {
            if let Some(value) = object.get(key) {
                let Some(schemas) = value.as_object() else {
                    return false;
                };
                pending.extend(schemas.values());
            }
        }
        if let Some(value) = object.get("dependentRequired") {
            let Some(dependencies) = value.as_object() else {
                return false;
            };
            if !dependencies.values().all(unique_string_array) {
                return false;
            }
        }
        if let Some(value) = object.get("dependencies") {
            let Some(dependencies) = value.as_object() else {
                return false;
            };
            for value in dependencies.values() {
                if value.is_array() {
                    if !unique_string_array(value) {
                        return false;
                    }
                } else {
                    pending.push(value);
                }
            }
        }
        for key in [
            "additionalProperties",
            "additionalItems",
            "unevaluatedProperties",
            "unevaluatedItems",
            "propertyNames",
            "contains",
            "contentSchema",
            "not",
            "if",
            "then",
            "else",
        ] {
            if let Some(value) = object.get(key) {
                pending.push(value);
            }
        }
        if let Some(items) = object.get("items") {
            match items {
                Value::Array(items) => pending.extend(items), // draft-07 tuple form
                _ => pending.push(items),
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(value) = object.get(key) {
                let Some(schemas) = value.as_array().filter(|schemas| !schemas.is_empty()) else {
                    return false;
                };
                pending.extend(schemas);
            }
        }
    }
    has_form_properties(root)
}

// Reject a syntactically valid but empty/free-form schema: it cannot describe
// the Service form. Follow local refs and composition without recursing forever.
fn has_form_properties(root: &Value) -> bool {
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(value) = pending.pop() {
        if !seen.insert(value as *const Value) {
            continue;
        }
        let Some(object) = value.as_object() else {
            continue;
        };
        let supports_object = match object.get("type") {
            None => true,
            Some(Value::String(value)) => value == "object",
            Some(Value::Array(values)) => values.iter().any(|v| v.as_str() == Some("object")),
            _ => false,
        };
        if !supports_object {
            continue;
        }
        if object
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty())
        {
            return true;
        }
        if let Some(target) = object
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| schema_reference(root, r))
        {
            pending.push(target);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(children) = object.get(key).and_then(Value::as_array) {
                pending.extend(children);
            }
        }
    }
    false
}

fn parse_schema(bytes: &[u8]) -> Result<Map<String, Value>, ApiError> {
    let invalid = || unavailable("committed KCL service form schema is absent or invalid");
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if !valid_schema(&value) {
        return Err(invalid());
    }
    match value {
        Value::Object(schema) => Ok(schema),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, path::PathBuf};

    #[test]
    fn committed_source_keeps_only_original_json_values_and_service_identity() {
        let source = parse_service(
            b"name: selected\ndeployments:\n  - name: a\n    image: old@sha256:abc\ncustom: {nested: [true, null, 3]}\n",
            "selected",
        ).unwrap();
        assert_eq!(source["deployments"][0]["image"], "old@sha256:abc");
        assert_eq!(source["custom"], json!({"nested": [true, null, 3]}));
        assert!(source["deployments"][0].get("replicas").is_none());
        assert!(parse_service(b"name: other\n", "selected").is_err());
        assert!(parse_service(b"[one, two]", "selected").is_err());
        assert!(parse_service(b"name: selected\nvalue: .inf\n", "selected").is_err());
        assert!(parse_service(b"name: selected\nvalue: !tag foo\n", "selected").is_err());
        assert!(parse_service(b"name: selected\nvalue: {1: a}\n", "selected").is_err());
    }

    #[test]
    fn artifact_choices_preserve_committed_fields_and_require_uris() {
        let artifacts = parse_artifacts(
            b"model:\n  uri: oci://registry/model@sha256:abc\n  modelDigest: sha256:123\n  tokenizerDigest: sha256:456\n  metadata: {variant: original}\n",
        ).unwrap();
        assert_eq!(artifacts["model"]["uri"], "oci://registry/model@sha256:abc");
        assert_eq!(artifacts["model"]["modelDigest"], "sha256:123");
        assert_eq!(artifacts["model"]["tokenizerDigest"], "sha256:456");
        assert_eq!(
            artifacts["model"]["metadata"],
            json!({"variant":"original"})
        );
        assert_eq!(parse_artifacts(b"{}").unwrap().len(), 0);
        assert!(parse_artifacts(b"model: {modelDigest: sha256:123}").is_err());
        assert!(parse_artifacts(b"model: {uri: null}").is_err());
        assert!(parse_artifacts(b"model: {uri: ''}").is_err());
        assert!(parse_artifacts(b"model: {uri: !tag foo}").is_err());
        assert!(parse_artifacts(b"[model]").is_err());
    }

    #[test]
    fn schema_keeps_generated_definitions_and_requires_usable_local_refs() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/Service",
            "$defs": {
                "Service": {"type":"object", "properties": {
                    "deployments": {"type":"array", "items":{"$ref":"#/$defs/Deployment"}}
                }},
                "Deployment": {"allOf":[{"type":"object", "properties":{
                    "image":{"type":"string"}
                }}]}
            }
        });
        let result = parse_schema(&serde_json::to_vec(&schema).unwrap()).unwrap();
        assert_eq!(Value::Object(result), schema);
        for invalid in [
            json!({}),
            json!({"type":"object", "properties":{"x":{"type":"made-up"}}}),
            json!({"type":"object", "properties":{"x":{"$ref":"#/$defs/missing"}}}),
            json!({"type":"object", "properties":{"x":{"$ref":"https://example.test/schema"}}}),
            json!({"type":"object", "properties":{"x":{"items":3}}}),
            json!({"type":"object", "properties":{"x":{}}, "required":["x", "x"]}),
            json!({"type":"object", "properties":{"x":{}}, "allOf":[]}),
            json!({"type":"object", "properties":{"x":{"$dynamicRef":"#"}}}),
            json!({"type":"object", "properties":{"x":{"$id":"nested", "$ref":"#"}}}),
            json!({"$ref":"#/components/schemas/Service", "components":{"schemas":{
                "Service":{"type":"object", "properties":{"x":{"$ref":"#/missing"}}}
            }}}),
        ] {
            assert!(
                parse_schema(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
        assert!(parse_schema(b"not JSON").is_err());
    }

    #[test]
    fn recursive_local_schema_refs_are_supported_without_looping() {
        let schema = json!({
            "$ref":"#/definitions/Service",
            "definitions": {
                "Service":{"type":"object", "properties":{"child":{"$ref":"#/definitions/Service"}}}
            }
        });
        assert!(valid_schema(&schema));
        assert!(!valid_schema(&json!({"$ref":"#"})));
    }

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(directory: &FsPath, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn blob_reads_are_commit_pinned_literal_and_regular() {
        let temp =
            Scratch(std::env::temp_dir().join(format!("thalamus-form-{}", uuid::Uuid::new_v4())));
        fs::create_dir_all(temp.0.join("catalog/services")).unwrap();
        git(&temp.0, &["init", "--quiet"]);
        fs::write(temp.0.join("catalog/services/s.yaml"), "name: before\n").unwrap();
        fs::write(
            temp.0.join("catalog/artifacts.yaml"),
            "model: {uri: 'oci://before'}\n",
        )
        .unwrap();
        fs::write(temp.0.join("schema.json"), "{\"type\":\"object\"}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("schema.json", temp.0.join("link.json")).unwrap();
        git(&temp.0, &["add", "."]);
        git(
            &temp.0,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let commit = git(&temp.0, &["rev-parse", "HEAD"]);
        fs::write(temp.0.join("catalog/services/s.yaml"), "name: after\n").unwrap();
        fs::write(
            temp.0.join("catalog/artifacts.yaml"),
            "model: {uri: 'oci://after'}\n",
        )
        .unwrap();
        let repo = temp.0.join(".git");
        assert_eq!(
            read_regular_blob(&repo, &commit, FsPath::new("catalog/services/s.yaml")).unwrap(),
            b"name: before\n"
        );
        let committed_artifacts =
            read_regular_blob(&repo, &commit, FsPath::new("catalog/artifacts.yaml")).unwrap();
        assert_eq!(
            parse_artifacts(&committed_artifacts).unwrap()["model"]["uri"],
            "oci://before"
        );
        assert!(read_regular_blob(&repo, &commit, FsPath::new("catalog/services/*.yaml")).is_err());
        assert!(read_regular_blob(&repo, &commit, FsPath::new("../schema.json")).is_err());
        assert!(read_regular_blob(&repo, "HEAD", FsPath::new("schema.json")).is_err());
        #[cfg(unix)]
        assert!(read_regular_blob(&repo, &commit, FsPath::new("link.json")).is_err());
    }
}
