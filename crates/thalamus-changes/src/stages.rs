//! Transform reviewed deployment edits into isolated candidate/promotion/cleanup
//! Git patches. These functions only read the configured Git worktree.
use super::*;
use serde_yaml::Value;
use std::path::{Component, Path};
use thalamus_gitops::Operation;

pub(crate) struct Prepared {
    pub patches: Vec<Patch>,
    pub old: Option<String>,
    pub candidate: Option<String>,
    pub revision: Option<String>,
    pub in_place_pool: bool,
    pub previous_revision: Option<String>,
    pub workload_namespace: Option<String>,
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Error::Invalid("deployment field is missing"))
}
fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}
fn deployments(doc: &Value) -> Result<&Vec<Value>> {
    doc.get("deployments")
        .and_then(Value::as_sequence)
        .ok_or(Error::Invalid("service deployments must be a list"))
}
fn read_yaml(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).map_err(|_| Error::GitOps)?;
    serde_yaml::from_slice(&bytes).map_err(|_| Error::Invalid("invalid catalog YAML"))
}
pub(crate) fn service_file(change: &Change) -> Result<PathBuf> {
    if change.request.revert_commit.is_some() {
        return Err(Error::Invalid("rollout deployment edit cannot be a revert"));
    }
    let mut service: Option<PathBuf> = None;
    let mut artifacts = 0usize;
    for patch in &change.request.patches {
        if patch.file == Path::new("catalog/artifacts.yaml") {
            artifacts += 1;
            if artifacts > 1
                || patch.path.len() != 1
                || patch.path[0].is_empty()
                || patch.path[0].len() > 253
                || patch.path[0].chars().any(char::is_control)
                || !matches!(&patch.operation, Operation::Set(Value::Mapping(_)))
            {
                return Err(Error::Invalid(
                    "artifact creation requires one named mapping",
                ));
            }
        } else {
            let file = &patch.file;
            if file
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
                || file.parent() != Some(Path::new("catalog/services"))
                || !matches!(
                    file.extension().and_then(|s| s.to_str()),
                    Some("yaml" | "yml")
                )
                || service.as_ref().is_some_and(|existing| existing != file)
            {
                return Err(Error::Invalid("rollout requires one service catalog file"));
            }
            service = Some(file.clone());
        }
    }
    service.ok_or(Error::Invalid("missing service patch"))
}
fn role(value: &Value) -> &str {
    value
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("stable")
}
fn weight(value: &Value) -> i64 {
    value.get("weight").and_then(Value::as_i64).unwrap_or(1)
}
fn set(value: &mut Value, key: &str, new: Value) -> Result<()> {
    value
        .as_mapping_mut()
        .ok_or(Error::Invalid("deployment must be a mapping"))?
        .insert(text(key), new);
    Ok(())
}
fn revision(root: &Path, deployment: &Value) -> Result<String> {
    let mut identity = deployment.clone();
    let map = identity
        .as_mapping_mut()
        .ok_or(Error::Invalid("deployment must be a mapping"))?;
    for key in ["revision", "role", "weight"] {
        map.remove(text(key));
    }
    let artifact = field(&identity, "artifact")?;
    let hardware = field(&identity, "hardware")?;
    let artifacts = read_yaml(&root.join("catalog/artifacts.yaml"))?;
    let hardware_catalog = read_yaml(&root.join("catalog/hardware.yaml"))?;
    let artifact_data = artifacts
        .get(artifact)
        .ok_or(Error::Invalid("unknown deployment artifact"))?;
    let hardware_data = hardware_catalog
        .get(hardware)
        .ok_or(Error::Invalid("unknown deployment hardware"))?;
    // serde_json maps are sorted, so equivalent mapping order yields the same revision.
    let identity = serde_json::json!({"deployment":identity,"artifact":artifact_data,"hardware":hardware_data});
    Ok(digest(&serde_json::to_vec(&identity)?))
}

fn ensure_unique_names(root: &Path, service: &Value, candidate: Option<&str>) -> Result<()> {
    let namespace = field(service, "namespace")?;
    let service_name = field(service, "name")?;
    let gateway = service.get("gateway").and_then(|g| {
        Some((
            g.get("namespace")?.as_str()?,
            g.get("name")?.as_str()?,
            g.get("hostname")?.as_str()?,
        ))
    });
    let mut service_names = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(root.join("catalog/services")).map_err(|_| Error::GitOps)? {
        let entry = entry.map_err(|_| Error::GitOps)?;
        let file_type = entry.file_type().map_err(|_| Error::GitOps)?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        ) {
            continue;
        }
        let doc = read_yaml(&path)?;
        let other_name = field(&doc, "name")?;
        if !service_names.insert(other_name.to_owned()) {
            return Err(Error::Conflict("service names collide in the catalog"));
        }
        let other_gateway = doc.get("gateway").and_then(|g| {
            Some((
                g.get("namespace")?.as_str()?,
                g.get("name")?.as_str()?,
                g.get("hostname")?.as_str()?,
            ))
        });
        if other_name != service_name && gateway.is_some() && gateway == other_gateway {
            return Err(Error::Conflict(
                "public gateway hostname is already used by another service",
            ));
        }
        if field(&doc, "namespace")? != namespace {
            continue;
        }
        for deployment in deployments(&doc)? {
            let name = field(deployment, "name")?;
            if !names.insert(name.to_owned()) {
                return Err(Error::Conflict(
                    "deployment names collide in the workload namespace",
                ));
            }
        }
    }
    if candidate.is_some_and(|candidate| names.contains(candidate)) {
        return Err(Error::Conflict("candidate deployment name already exists"));
    }
    Ok(())
}
pub(crate) fn check_artifact_creation(root: &Path, change: &Change) -> Result<()> {
    if change.request.revert_commit.is_some() {
        return Ok(());
    }
    service_file(change)?;
    for patch in &change.request.patches {
        if patch.file == Path::new("catalog/artifacts.yaml") {
            let artifacts = read_yaml(&root.join(&patch.file))?;
            if artifacts.get(patch.path[0].as_str()).is_some() {
                return Err(Error::Conflict(
                    "artifact already exists; choose a new name",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn check_preview_names(root: &Path, change: &Change) -> Result<()> {
    if change.request.revert_commit.is_some() {
        return Ok(());
    }
    let file = service_file(change)?;
    let doc = read_yaml(&root.join(file))?;
    // Non-serving catalog fixtures can lack deployments; real serving schemas
    // require both namespace and the deployment list.
    if doc.get("deployments").is_some() {
        ensure_unique_names(root, &doc, None)?;
    }
    Ok(())
}

pub(crate) fn public_target(
    repo: &GitOpsRepo,
    config: &WorkerConfig,
    change: &Change,
    commit: &str,
) -> Result<(String, String)> {
    let file = repo
        .service_file(commit, &change.request.service)
        .map_err(|_| Error::GitOps)?;
    let path = config
        .worktree_root
        .join(format!("public-target-{}", Uuid::new_v4()));
    let tree = repo
        .create_worktree(commit, &path)
        .map_err(|_| Error::GitOps)?;
    let result = (|| {
        let doc = read_yaml(&tree.path().join(file))?;
        let candidates: Vec<_> = deployments(&doc)?
            .iter()
            .filter(|d| role(d) == "stable" && weight(d) > 0)
            .collect();
        if candidates.len() != 1 {
            return Err(Error::Invalid(
                "public validation requires one stable backend",
            ));
        }
        let name = field(candidates[0], "name")?.to_owned();
        let expected = field(candidates[0], "revision")?.to_owned();
        if revision(tree.path(), candidates[0])? != expected {
            return Err(Error::Invalid(
                "public deployment revision does not match its configuration",
            ));
        }
        Ok((name, expected))
    })();
    let cleanup = repo.discard_worktree(tree).map_err(|_| Error::GitOps);
    match (result, cleanup) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}

pub(crate) fn prepare(
    change: &Change,
    repo: &GitOpsRepo,
    config: &PreviewConfig,
) -> Result<Prepared> {
    let file = service_file(change)?;
    repo.fetch().map_err(|_| Error::GitOps)?;
    if repo
        .resolve_base(&change.request.target_branch)
        .map_err(|_| Error::GitOps)?
        != change.request.base_sha
    {
        return Err(Error::Invalid("target base moved; refresh the proposal"));
    }
    if repo
        .service_file(&change.request.base_sha, &change.request.service)
        .map_err(|_| Error::GitOps)?
        != file
    {
        return Err(Error::Invalid(
            "patch does not address the configured service",
        ));
    }
    let path = config
        .worktree_root
        .join(format!("prepare-{}-{}", change.id, Uuid::new_v4()));
    let mut tree = repo
        .create_worktree(&change.request.base_sha, &path)
        .map_err(|_| Error::GitOps)?;
    let result = (|| {
        let base = read_yaml(&tree.path().join(&file))?;
        if field(&base, "name")? != change.request.service {
            return Err(Error::Invalid("service identity does not match"));
        }
        check_artifact_creation(tree.path(), change)?;
        tree.preview(&change.request.patches, &config.render)
            .map_err(|_| Error::GitOps)?;
        let desired = read_yaml(&tree.path().join(&file))?;
        if field(&desired, "name")? != change.request.service {
            return Err(Error::Invalid("service identity cannot change"));
        }
        let mut base_other = base.clone();
        let mut desired_other = desired.clone();
        for doc in [&mut base_other, &mut desired_other] {
            doc.as_mapping_mut()
                .ok_or(Error::Invalid("service must be a mapping"))?
                .remove(text("deployments"));
        }
        if base_other != desired_other {
            return Err(Error::Invalid("rollout must change one deployment only"));
        }
        let old = deployments(&base)?;
        let new = deployments(&desired)?;
        if old.len() != new.len() || old.is_empty() {
            return Err(Error::Invalid(
                "rollout must update one existing deployment",
            ));
        }
        let mut changed = None;
        for (i, (before, after)) in old.iter().zip(new).enumerate() {
            if field(before, "name")? != field(after, "name")? {
                return Err(Error::Invalid("deployment identity cannot change"));
            }
            if before != after && changed.replace(i).is_some() {
                return Err(Error::Invalid("rollout must update one deployment"));
            }
        }
        let index = changed.ok_or(Error::Invalid("deployment did not change"))?;
        if role(&old[index]) != "stable"
            || role(&new[index]) != "stable"
            || weight(&old[index]) <= 0
        {
            return Err(Error::Invalid("rollout requires a live stable deployment"));
        }
        let candidate_name = format!("candidate-{}", &change.id.simple().to_string()[..29]);
        ensure_unique_names(
            tree.path(),
            &base,
            matches!(&change.request.rollout, Rollout::Gated).then_some(candidate_name.as_str()),
        )?;
        match &change.request.rollout {
            Rollout::Gated => {
                if old
                    .iter()
                    .enumerate()
                    .any(|(i, d)| i != index && role(d) == "stable" && weight(d) > 0)
                {
                    return Err(Error::Invalid(
                        "gated promotion requires one public backend",
                    ));
                }
                let old_name = field(&old[index], "name")?.to_owned();
                let candidate = candidate_name;
                let mut deployment = new[index].clone();
                set(&mut deployment, "name", text(&candidate))?;
                set(&mut deployment, "role", text("candidate"))?;
                set(&mut deployment, "weight", Value::Number(1.into()))?;
                if base.get("pool").is_some() {
                    let member = old[index]
                        .get("poolMember")
                        .and_then(Value::as_str)
                        .unwrap_or(&old_name);
                    set(&mut deployment, "poolMember", text(member))?;
                }
                set(&mut deployment, "rolloutStrategy", text("RollingUpdate"))?;
                let revision = revision(tree.path(), &deployment)?;
                set(&mut deployment, "revision", text(&revision))?;
                let mut result = old.clone();
                result.push(deployment);
                let mut patches: Vec<Patch> = change
                    .request
                    .patches
                    .iter()
                    .filter(|p| p.file == Path::new("catalog/artifacts.yaml"))
                    .cloned()
                    .collect();
                patches.push(Patch {
                    file,
                    path: vec!["deployments".into()],
                    operation: Operation::Set(Value::Sequence(result)),
                });
                Ok(Prepared {
                    patches,
                    old: Some(old_name),
                    candidate: Some(candidate),
                    revision: Some(revision),
                    in_place_pool: false,
                    previous_revision: None,
                    workload_namespace: None,
                })
            }
            Rollout::InPlace { .. } => {
                let pooled = base.get("pool").is_some();
                if old
                    .iter()
                    .enumerate()
                    .any(|(i, d)| i != index && role(d) == "stable" && weight(d) > 0)
                {
                    return Err(Error::Invalid(
                        "in-place rollout requires one public backend",
                    ));
                }
                if !pooled
                    && old[index].get("rolloutStrategy").and_then(Value::as_str) != Some("Recreate")
                {
                    return Err(Error::Invalid(
                        "in-place requires an existing raw Recreate workload; migrate separately",
                    ));
                }
                let mut deployment = new[index].clone();
                set(&mut deployment, "rolloutStrategy", text("Recreate"))?;
                let revision = revision(tree.path(), &deployment)?;
                set(&mut deployment, "revision", text(&revision))?;
                let mut patches = change.request.patches.clone();
                patches.push(Patch {
                    file,
                    path: vec!["deployments".into(), index.to_string()],
                    operation: Operation::Set(deployment),
                });
                Ok(Prepared {
                    patches,
                    old: Some(field(&old[index], "name")?.to_owned()),
                    candidate: Some(field(&old[index], "name")?.to_owned()),
                    revision: Some(revision),
                    in_place_pool: pooled,
                    previous_revision: if pooled {
                        Some(field(&old[index], "revision")?.to_owned())
                    } else {
                        None
                    },
                    workload_namespace: if pooled {
                        Some(field(&base, "namespace")?.to_owned())
                    } else {
                        None
                    },
                })
            }
            Rollout::Rolling => {
                if old[index].get("rolloutStrategy").and_then(Value::as_str) == Some("Recreate")
                    || new[index].get("rolloutStrategy").and_then(Value::as_str) == Some("Recreate")
                {
                    return Err(Error::Invalid(
                        "rolling cannot change serving controller kind",
                    ));
                }
                let mut deployment = new[index].clone();
                set(&mut deployment, "rolloutStrategy", text("RollingUpdate"))?;
                let revision = revision(tree.path(), &deployment)?;
                set(&mut deployment, "revision", text(&revision))?;
                let mut patches = change.request.patches.clone();
                patches.push(Patch {
                    file,
                    path: vec!["deployments".into(), index.to_string()],
                    operation: Operation::Set(deployment),
                });
                Ok(Prepared {
                    patches,
                    old: None,
                    candidate: Some(field(&old[index], "name")?.to_owned()),
                    revision: Some(revision),
                    in_place_pool: false,
                    previous_revision: None,
                    workload_namespace: None,
                })
            }
        }
    })();
    let cleanup = repo.discard_worktree(tree).map_err(|_| Error::GitOps);
    match (result, cleanup) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}

pub(crate) fn next_stage_patches(
    repo: &GitOpsRepo,
    config: &WorkerConfig,
    change: &Change,
    status: &RolloutStatus,
    base: &str,
    promotion: bool,
) -> Result<Vec<Patch>> {
    let file = service_file(change)?;
    if repo
        .service_file(base, &change.request.service)
        .map_err(|_| Error::GitOps)?
        != file
    {
        return Err(Error::Invalid("service catalog path changed"));
    }
    let path = config
        .worktree_root
        .join(format!("stage-{}-{}", change.id, Uuid::new_v4()));
    let tree = repo
        .create_worktree(base, &path)
        .map_err(|_| Error::GitOps)?;
    let result = (|| {
        let doc = read_yaml(&tree.path().join(&file))?;
        if field(&doc, "name")? != change.request.service {
            return Err(Error::Invalid("service identity changed"));
        }
        let mut values = deployments(&doc)?.clone();
        let candidate = status
            .candidate_deployment
            .as_deref()
            .ok_or(Error::Invalid("candidate identity missing"))?;
        let old = status
            .old_deployment
            .as_deref()
            .ok_or(Error::Invalid("old deployment identity missing"))?;
        let expected_revision = status
            .candidate_revision
            .as_deref()
            .ok_or(Error::Invalid("candidate revision missing"))?;
        let indices: Vec<usize> = values
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                (d.get("name").and_then(Value::as_str) == Some(candidate)).then_some(i)
            })
            .collect();
        let old_indices: Vec<usize> = values
            .iter()
            .enumerate()
            .filter_map(|(i, d)| (d.get("name").and_then(Value::as_str) == Some(old)).then_some(i))
            .collect();
        if indices.len() != 1 || old_indices.len() != 1 {
            return Err(Error::Invalid("rollout deployment identities changed"));
        }
        let c = indices[0];
        let o = old_indices[0];
        if field(&values[c], "revision")? != expected_revision
            || revision(tree.path(), &values[c])? != expected_revision
        {
            return Err(Error::Invalid(
                "candidate configuration or revision changed",
            ));
        }
        if promotion {
            if role(&values[c]) != "candidate"
                || role(&values[o]) != "stable"
                || weight(&values[o]) <= 0
                || values
                    .iter()
                    .enumerate()
                    .any(|(i, d)| i != o && role(d) == "stable" && weight(d) > 0)
            {
                return Err(Error::Invalid("routing changed before promotion"));
            }
            set(&mut values[c], "role", text("stable"))?;
            set(&mut values[c], "weight", Value::Number(1.into()))?;
            set(&mut values[o], "weight", Value::Number(0.into()))?;
        } else {
            if role(&values[c]) != "stable"
                || weight(&values[c]) <= 0
                || role(&values[o]) != "stable"
                || weight(&values[o]) != 0
                || values
                    .iter()
                    .enumerate()
                    .any(|(i, d)| i != c && role(d) == "stable" && weight(d) > 0)
            {
                return Err(Error::Invalid("routing changed before cleanup"));
            }
            values.remove(o);
        }
        Ok(vec![Patch {
            file,
            path: vec!["deployments".into()],
            operation: Operation::Set(Value::Sequence(values)),
        }])
    })();
    let cleanup = repo.discard_worktree(tree).map_err(|_| Error::GitOps);
    match (result, cleanup) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}

/// Both approved renders must contain one explicitly zeroed filler Deployment.
pub(crate) fn zero_filler_render(
    rendered: &str,
    namespace: &str,
    deployment: &str,
) -> Result<bool> {
    fn visit(value: &Value, namespace: &str, name: &str, matches: &mut usize, zero: &mut bool) {
        match value {
            Value::Mapping(map) => {
                if map.get(text("kind")).and_then(Value::as_str) == Some("Deployment") {
                    let meta = map.get(text("metadata"));
                    if meta.and_then(|m| m.get("name")).and_then(Value::as_str) == Some(name)
                        && meta
                            .and_then(|m| m.get("namespace"))
                            .and_then(Value::as_str)
                            == Some(namespace)
                    {
                        *matches += 1;
                        *zero &= map.get(text("apiVersion")).and_then(Value::as_str)
                            == Some("apps/v1")
                            && map
                                .get(text("spec"))
                                .and_then(|s| s.get("replicas"))
                                .and_then(Value::as_u64)
                                == Some(0);
                    }
                }
                let kind = map.get(text("kind")).and_then(Value::as_str);
                let meta = map.get(text("metadata"));
                if matches!(kind, Some("HorizontalPodAutoscaler" | "ScaledObject"))
                    && meta
                        .and_then(|m| m.get("namespace"))
                        .and_then(Value::as_str)
                        == Some(namespace)
                    && map
                        .get(text("spec"))
                        .and_then(|s| s.get("scaleTargetRef"))
                        .and_then(|s| s.get("name"))
                        .and_then(Value::as_str)
                        == Some(name)
                {
                    *zero = false;
                }
                for (_, child) in map {
                    visit(child, namespace, name, matches, zero);
                }
            }
            Value::Sequence(values) => {
                for child in values {
                    visit(child, namespace, name, matches, zero);
                }
            }
            _ => {}
        }
    }
    let name = format!("{deployment}-filler");
    let (mut count, mut zero) = (0usize, true);
    for document in serde_yaml::Deserializer::from_str(rendered) {
        let value =
            Value::deserialize(document).map_err(|_| Error::Invalid("render is not valid YAML"))?;
        visit(&value, namespace, &name, &mut count, &mut zero);
    }
    Ok(count == 1 && zero)
}

fn at_path<'a>(mut value: &'a Value, path: &[String]) -> Option<&'a Value> {
    for part in path {
        value = match value {
            Value::Mapping(m) => m.get(text(part))?,
            Value::Sequence(s) => s.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(value)
}

fn dependencies(
    value: &Value,
    artifacts: &mut std::collections::BTreeSet<String>,
    hardware: &mut std::collections::BTreeSet<String>,
) {
    match value {
        Value::Mapping(map) => {
            if let Some(name) = map.get(text("artifact")).and_then(Value::as_str) {
                artifacts.insert(name.to_owned());
            }
            if let Some(name) = map.get(text("hardware")).and_then(Value::as_str) {
                hardware.insert(name.to_owned());
            }
            for (_, child) in map {
                dependencies(child, artifacts, hardware);
            }
        }
        Value::Sequence(values) => {
            for child in values {
                dependencies(child, artifacts, hardware);
            }
        }
        _ => {}
    }
}
/// Reject refreshes that overwrite a concurrently modified patch target.
pub(crate) fn ensure_refresh_safe(
    change: &Change,
    repo: &GitOpsRepo,
    config: &PreviewConfig,
    new_base: &str,
) -> Result<()> {
    if change.request.revert_commit.is_some() {
        return Ok(());
    }
    let old_path = config
        .worktree_root
        .join(format!("refresh-old-{}", Uuid::new_v4()));
    let new_path = config
        .worktree_root
        .join(format!("refresh-new-{}", Uuid::new_v4()));
    let old = repo
        .create_worktree(&change.request.base_sha, &old_path)
        .map_err(|_| Error::GitOps)?;
    let new = match repo.create_worktree(new_base, &new_path) {
        Ok(tree) => tree,
        Err(_) => {
            repo.discard_worktree(old).map_err(|_| Error::GitOps)?;
            return Err(Error::GitOps);
        }
    };
    let result = (|| {
        let mut documents = std::collections::BTreeMap::new();
        for patch in &change.request.patches {
            if !documents.contains_key(&patch.file) {
                let before = read_yaml(&old.path().join(&patch.file))?;
                let after = read_yaml(&new.path().join(&patch.file))?;
                documents.insert(patch.file.clone(), (before, after));
            }
            let (before, after) = &documents[&patch.file];
            if at_path(before, &patch.path) != at_path(after, &patch.path) {
                return Err(Error::Conflict(
                    "refresh conflicts with changes to a patched path; re-propose",
                ));
            }
        }
        let mut artifacts = std::collections::BTreeSet::new();
        let mut hardware = std::collections::BTreeSet::new();
        for (before, _) in documents.values() {
            dependencies(before, &mut artifacts, &mut hardware);
        }
        for patch in &change.request.patches {
            if let Operation::Set(value) = &patch.operation {
                dependencies(value, &mut artifacts, &mut hardware);
            }
        }
        for (path, names) in [
            ("catalog/artifacts.yaml", artifacts),
            ("catalog/hardware.yaml", hardware),
        ] {
            if names.is_empty() {
                continue;
            }
            let before = read_yaml(&old.path().join(path))?;
            let after = read_yaml(&new.path().join(path))?;
            for name in names {
                if before.get(name.as_str()) != after.get(name.as_str()) {
                    return Err(Error::Conflict(
                        "refresh changed a referenced artifact or hardware profile; re-propose",
                    ));
                }
            }
        }
        Ok(())
    })();
    let first = repo.discard_worktree(old).map_err(|_| Error::GitOps);
    let second = repo.discard_worktree(new).map_err(|_| Error::GitOps);
    result.and(first).and(second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn revision_binds_artifact_hardware_and_serving_configuration_not_route_role() {
        let root = std::env::temp_dir().join(format!("rollout-revision-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("catalog")).unwrap();
        fs::write(
            root.join("catalog/artifacts.yaml"),
            "model:\n  uri: oci://image@sha256:abc\n",
        )
        .unwrap();
        fs::write(root.join("catalog/hardware.yaml"), "gpu:\n  gpuCount: 1\n").unwrap();
        let mut deployment: Value = serde_yaml::from_str(
            "name: candidate-123\nartifact: model\nhardware: gpu\nimage: image@sha256:abc\nrole: candidate\nweight: 1\nrevision: old\n").unwrap();
        let original = revision(&root, &deployment).unwrap();
        set(&mut deployment, "role", text("stable")).unwrap();
        set(&mut deployment, "weight", Value::Number(0.into())).unwrap();
        set(&mut deployment, "revision", text("different")).unwrap();
        assert_eq!(original, revision(&root, &deployment).unwrap());
        set(&mut deployment, "image", text("image@sha256:def")).unwrap();
        assert_ne!(original, revision(&root, &deployment).unwrap());
        fs::write(root.join("catalog/hardware.yaml"), "gpu:\n  gpuCount: 2\n").unwrap();
        let changed = revision(&root, &deployment).unwrap();
        fs::write(
            root.join("catalog/artifacts.yaml"),
            "model:\n  uri: oci://other@sha256:def\n",
        )
        .unwrap();
        assert_ne!(changed, revision(&root, &deployment).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_are_unique_across_services_in_the_same_namespace() {
        let root = std::env::temp_dir().join(format!("rollout-names-{}", Uuid::new_v4()));
        let services = root.join("catalog/services");
        fs::create_dir_all(&services).unwrap();
        fs::write(
            services.join("one.yaml"),
            "name: one\nnamespace: ns\ndeployments:\n- name: existing\n",
        )
        .unwrap();
        fs::write(
            services.join("two.yml"),
            "name: two\nnamespace: other\ndeployments:\n- name: existing\n",
        )
        .unwrap();
        let service = read_yaml(&services.join("one.yaml")).unwrap();
        assert!(ensure_unique_names(&root, &service, Some("candidate-new")).is_ok());
        assert!(matches!(
            ensure_unique_names(&root, &service, Some("existing")),
            Err(Error::Conflict(_))
        ));
        fs::write(
            services.join("two.yml"),
            "name: two\nnamespace: ns\ndeployments:\n- name: existing\n",
        )
        .unwrap();
        assert!(matches!(
            ensure_unique_names(&root, &service, None),
            Err(Error::Conflict(_))
        ));
        fs::write(services.join("one.yaml"),"name: one\nnamespace: ns\ngateway: {namespace: gw, name: api, hostname: same.example}\ndeployments:\n- name: first\n").unwrap();
        fs::write(services.join("two.yml"),"name: two\nnamespace: other\ngateway: {namespace: gw, name: api, hostname: same.example}\ndeployments:\n- name: second\n").unwrap();
        let service = read_yaml(&services.join("one.yaml")).unwrap();
        assert!(matches!(
            ensure_unique_names(&root, &service, None),
            Err(Error::Conflict(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_paths_distinguish_missing_and_changed_values() {
        let first: Value = serde_yaml::from_str("deployments:\n- name: a\n  image: one\n").unwrap();
        let second: Value =
            serde_yaml::from_str("deployments:\n- name: a\n  image: two\n").unwrap();
        let image = vec!["deployments".into(), "0".into(), "image".into()];
        let name = vec!["deployments".into(), "0".into(), "name".into()];
        assert_ne!(at_path(&first, &image), at_path(&second, &image));
        assert_eq!(at_path(&first, &name), at_path(&second, &name));
        assert!(at_path(&first, &["missing".into()]).is_none());
    }

    #[test]
    fn in_place_render_requires_zero_filler_and_no_scaler() {
        let zero = "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: model-filler, namespace: ns}\nspec: {replicas: 0}\n";
        assert!(zero_filler_render(zero, "ns", "model").unwrap());
        assert!(
            !zero_filler_render(&zero.replace("replicas: 0", "replicas: 1"), "ns", "model")
                .unwrap()
        );
        assert!(!zero_filler_render(zero, "wrong", "model").unwrap());
        let scaler = format!("{zero}---\napiVersion: keda.sh/v1alpha1\nkind: ScaledObject\nmetadata: {{name: scaler, namespace: ns}}\nspec: {{scaleTargetRef: {{name: model-filler}}}}\n");
        assert!(!zero_filler_render(&scaler, "ns", "model").unwrap());
        assert!(!zero_filler_render(&format!("{zero}---\n{zero}"), "ns", "model").unwrap());
    }

    #[test]
    fn real_git_fixture_builds_candidate_promotion_and_cleanup_patches() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git fixture command failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }
        fn sequence(patch: &Patch) -> Vec<Value> {
            match &patch.operation {
                Operation::Set(Value::Sequence(v)) => v.clone(),
                _ => panic!("expected deployments patch"),
            }
        }
        let root = std::env::temp_dir().join(format!("rollout-stages-{}", Uuid::new_v4()));
        let source = root.join("source");
        fs::create_dir_all(source.join("catalog/services")).unwrap();
        fs::write(
            source.join("catalog/artifacts.yaml"),
            "model:\n  uri: oci://model\n",
        )
        .unwrap();
        fs::write(
            source.join("catalog/hardware.yaml"),
            "gpu:\n  gpuCount: 1\n",
        )
        .unwrap();
        let file = PathBuf::from("catalog/services/example.yaml");
        fs::write(
            source.join(&file),
            format!(
                "name: example\nnamespace: ns\nnetwork:\n  gatewayPodSelector: {{app: gateway}}\n  dns: {{namespaceSelector: {{role: dns}}, podSelector: {{app: dns}}}}\n  prometheus: {{namespaceSelector: {{role: monitoring}}, podSelector: {{app: prometheus}}}}\n  apiServer: [{{cidr: 192.0.2.0/24, ports: [6443]}}]\n  storageEgress: []\ndeployments:\n- name: old\n  artifact: model\n  hardware: gpu\n  image: old\n  weight: 1\n  revision: {}\n",
                "a".repeat(64)
            ),
        ).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["config", "user.email", "test@example.test"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let bare = root.join("bare.git");
        let unbound = GitOpsRepo::clone_bare(source.as_os_str(), &bare).unwrap();
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let ssh = root.join("ssh-transport");
        fs::write(&ssh,format!("#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\ncase \"$*\" in\n *git-upload-pack*) exec git upload-pack {} ;;\n *git-receive-pack*) exec git receive-pack {} ;;\n *) exit 2 ;;\nesac\n",
            quote(source.join(".git").to_str().unwrap()),quote(source.join(".git").to_str().unwrap()))).unwrap();
        git(
            &bare,
            &[
                "config",
                "core.sshCommand",
                &format!("sh {}", quote(ssh.to_str().unwrap())),
            ],
        );
        git(&bare, &["config", "ssh.variant", "ssh"]);
        let origin = "ssh://git@example.invalid/team/repository.git";
        git(&bare, &["remote", "set-url", "origin", origin]);
        let repo = unbound.bind_origin(origin, "main").unwrap();
        let worktree_root = root.join("worktrees");
        fs::create_dir_all(&worktree_root).unwrap();
        let render = RenderCommand {
            executable: "cat".into(),
            args: vec!["catalog/services/example.yaml".into()],
        };
        let preview = PreviewConfig {
            worktree_root: worktree_root.clone(),
            render: render.clone(),
        };
        let id = Uuid::new_v4();
        let mut change = Change {
            id,
            actor: "owner".into(),
            idempotency_key: "key".into(),
            request: Proposal {
                service: "example".into(),
                base_sha: base,
                target_branch: "main".into(),
                title: "update".into(),
                patches: vec![
                    Patch {
                        file: "catalog/artifacts.yaml".into(),
                        path: vec!["model-new".into()],
                        operation: Operation::Set(
                            serde_yaml::from_str("uri: oci://new-model").unwrap(),
                        ),
                    },
                    Patch {
                        file: file.clone(),
                        path: vec!["deployments".into(), "0".into(), "artifact".into()],
                        operation: Operation::Set(text("model-new")),
                    },
                    Patch {
                        file: file.clone(),
                        path: vec!["deployments".into(), "0".into(), "image".into()],
                        operation: Operation::Set(text("new")),
                    },
                ],
                revert_commit: None,
                validation_tier: "fixture".into(),
                rollout: Rollout::Gated,
            },
            state: "proposed".into(),
            branch: "fixture".into(),
            head_sha: None,
            pr_number: None,
            pr_url: None,
            base_render_sha256: None,
            proposed_render_sha256: None,
            data_diff: None,
            rendered_diff: None,
            error: None,
        };
        // Candidate staging may change one deployment, never the service's
        // shared network selectors or egress policy. The fixture renderer only
        // reads YAML, so this exercises the controller guard itself.
        let now = Utc::now();
        for rollout in [
            Rollout::Gated,
            Rollout::Rolling,
            Rollout::InPlace {
                window_start: now,
                window_end: now + chrono::Duration::hours(1),
            },
        ] {
            let mut unsafe_change = change.clone();
            unsafe_change.request.rollout = rollout;
            unsafe_change.request.patches.push(Patch {
                file: file.clone(),
                path: vec![
                    "network".into(),
                    "dns".into(),
                    "podSelector".into(),
                    "app".into(),
                ],
                operation: Operation::Set(text("other-dns")),
            });
            assert!(matches!(
                prepare(&unsafe_change, &repo, &preview),
                Err(Error::Invalid("rollout must change one deployment only"))
            ));
        }
        let prepared = prepare(&change, &repo, &preview).unwrap();
        let candidate_name = prepared.candidate.clone().unwrap();
        let candidate_revision = prepared.revision.clone().unwrap();
        assert_eq!(prepared.patches.len(), 2);
        assert_eq!(
            prepared.patches[0].file,
            Path::new("catalog/artifacts.yaml")
        );
        assert_eq!(prepared.patches[1].path, vec!["deployments".to_owned()]);
        let candidate_values = sequence(&prepared.patches[1]);
        assert_eq!(candidate_values.len(), 2);
        assert_eq!(field(&candidate_values[0], "image").unwrap(), "old");
        assert_eq!(field(&candidate_values[1], "image").unwrap(), "new");
        assert_eq!(
            field(&candidate_values[1], "artifact").unwrap(),
            "model-new"
        );
        assert_eq!(role(&candidate_values[1]), "candidate");
        assert_eq!(
            field(&candidate_values[1], "revision").unwrap(),
            candidate_revision
        );
        change.request.patches = prepared.patches;
        let mut doc = read_yaml(&source.join(&file)).unwrap();
        doc.as_mapping_mut()
            .unwrap()
            .insert(text("deployments"), Value::Sequence(candidate_values));
        fs::write(source.join(&file), serde_yaml::to_string(&doc).unwrap()).unwrap();
        let mut artifacts = read_yaml(&source.join("catalog/artifacts.yaml")).unwrap();
        artifacts.as_mapping_mut().unwrap().insert(
            text("model-new"),
            serde_yaml::from_str("uri: oci://new-model").unwrap(),
        );
        fs::write(
            source.join("catalog/artifacts.yaml"),
            serde_yaml::to_string(&artifacts).unwrap(),
        )
        .unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "candidate"]);
        let candidate_commit = git(&source, &["rev-parse", "HEAD"]);
        repo.fetch().unwrap();
        let status = RolloutStatus {
            change_id: id,
            stage: "promotion_pr".into(),
            prepared: true,
            window_start: None,
            window_end: None,
            candidate_deployment: Some(candidate_name),
            candidate_revision: Some(candidate_revision.clone()),
            old_deployment: Some("old".into()),
            in_place_pool: false,
            previous_revision: None,
            workload_namespace: None,
            filler_verified_at: None,
            candidate_validation_run: None,
            public_validation_run: None,
            queued: false,
            merge_requested_at: None,
            merge_observed_at: None,
            upstream_merged_at: None,
            error: None,
            approvals: vec![],
            stages: vec![],
        };
        let worker = WorkerConfig {
            worktree_root,
            render,
            commit_author: CommitAuthor {
                name: "Test".into(),
                email: "test@example.test".into(),
            },
            repository: forge::RepoRef::parse(forge::ForgeKind::Github, origin).unwrap(),
            forge_actor: "bot".into(),
        };
        let promotion =
            next_stage_patches(&repo, &worker, &change, &status, &candidate_commit, true).unwrap();
        let promoted = sequence(&promotion[0]);
        assert_eq!(weight(&promoted[0]), 0);
        assert_eq!(role(&promoted[1]), "stable");
        assert_eq!(field(&promoted[1], "revision").unwrap(), candidate_revision);
        doc.as_mapping_mut()
            .unwrap()
            .insert(text("deployments"), Value::Sequence(promoted));
        fs::write(source.join(&file), serde_yaml::to_string(&doc).unwrap()).unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "promotion"]);
        let promotion_commit = git(&source, &["rev-parse", "HEAD"]);
        repo.fetch().unwrap();
        let cleanup =
            next_stage_patches(&repo, &worker, &change, &status, &promotion_commit, false).unwrap();
        let remaining = sequence(&cleanup[0]);
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            field(&remaining[0], "revision").unwrap(),
            candidate_revision
        );
        fs::remove_dir_all(root).unwrap();
    }
}
