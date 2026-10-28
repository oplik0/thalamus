//! Worker-side reconciliation helpers. Leadership reduces contention; it is not fencing.
use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use thalamus_identity::ProjectionKey;
use thalamus_k8s::WatchState;

/// Encode only SHA-256 digests as documented by agentgateway's ConfigMap API.
/// The caller must obtain the entire authoritative snapshot before applying it.
pub fn gateway_entries(
    keys: &[ProjectionKey],
    now: DateTime<Utc>,
) -> Result<BTreeMap<String, String>, &'static str> {
    let mut entries = BTreeMap::new();
    for key in keys {
        let m = &key.metadata;
        if m.revoked_at.is_some() || m.expires_at.is_some_and(|at| at <= now) {
            continue;
        }
        if key.digest.len() != 32 {
            return Err("invalid key digest length");
        }
        let digest: String = key.digest.iter().map(|b| format!("{b:02x}")).collect();
        let entry = json!({"keyHash": format!("sha256:{digest}"), "metadata": {
            "key_id": m.id.to_string(), "user": m.user_id.to_string(),
            "creator": m.creator_id.to_string(), "team": m.team_id.to_string(),
            "project": m.project_id.map(|id| id.to_string()), "cost_centre": key.cost_centre,
            "kind": m.kind, "allowed_services": m.allowed_services,
            "budget": m.budget, "expiry": m.expires_at.map(|at| at.to_rfc3339())
        }});
        if entries
            .insert(m.id.to_string(), entry.to_string())
            .is_some()
        {
            return Err("duplicate key id");
        }
    }
    Ok(entries)
}

pub fn projection_digest(entries: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    for (key, value) in entries {
        hash.update((key.len() as u64).to_be_bytes());
        hash.update(key.as_bytes());
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// A watch is unknown immediately after error/relist and stale after max_age.
pub fn fresh(state: &WatchState, now: Instant, max_age: Duration) -> bool {
    state.is_fresh(now, max_age)
}

/// Only a complete Job condition is authoritative; counters are not completion.
pub fn job_outcome(job: &k8s_openapi::api::batch::v1::Job) -> Option<bool> {
    let conditions = job.status.as_ref()?.conditions.as_ref()?;
    if conditions
        .iter()
        .any(|c| c.type_ == "Failed" && c.status == "True")
    {
        return Some(false);
    }
    if conditions
        .iter()
        .any(|c| c.type_ == "Complete" && c.status == "True")
    {
        return Some(true);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use thalamus_identity::KeyMetadata;
    use uuid::Uuid;
    fn key() -> ProjectionKey {
        ProjectionKey {
            metadata: KeyMetadata {
                id: Uuid::new_v4(),
                name: "test".into(),
                user_id: Uuid::new_v4(),
                creator_id: Uuid::new_v4(),
                team_id: Uuid::new_v4(),
                project_id: None,
                kind: "personal".into(),
                allowed_services: vec!["model".into()],
                budget: Some(5),
                expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                revoked_at: None,
                created_at: Utc::now(),
            },
            digest: vec![0xab; 32],
            cost_centre: None,
        }
    }
    #[test]
    fn upstream_format_and_expiry() {
        let mut k = key();
        let entries = gateway_entries(&[key()], Utc::now()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(entries.values().next().unwrap()).unwrap();
        assert_eq!(value["keyHash"], format!("sha256:{}", "ab".repeat(32)));
        assert!(value.get("key").is_none());
        assert_eq!(value["metadata"]["allowed_services"][0], "model");
        k.metadata.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(gateway_entries(&[k], Utc::now()).unwrap().is_empty());
    }
    #[test]
    fn malformed_digest_fails_closed() {
        let mut k = key();
        k.digest.pop();
        assert!(gateway_entries(&[k], Utc::now()).is_err());
    }
    #[test]
    fn revoked_removed_and_unbounded_expiry_preserved() {
        let mut revoked = key();
        revoked.metadata.revoked_at = Some(Utc::now());
        let mut permanent = key();
        permanent.metadata.expires_at = None;
        let entries = gateway_entries(&[revoked, permanent], Utc::now()).unwrap();
        assert_eq!(entries.len(), 1);
        let value: serde_json::Value =
            serde_json::from_str(entries.values().next().unwrap()).unwrap();
        assert!(value["metadata"]["expiry"].is_null());
    }
    #[test]
    fn digest_is_stable_and_changes_with_value() {
        let mut entries = BTreeMap::from([("a".to_string(), "v".to_string())]);
        let digest = projection_digest(&entries);
        assert_eq!(digest, projection_digest(&entries.clone()));
        entries.insert("a".into(), "w".into());
        assert_ne!(digest, projection_digest(&entries));
    }
    #[test]
    fn job_conditions_not_counters() {
        let mut job = k8s_openapi::api::batch::v1::Job {
            status: Some(serde_json::from_value(json!({"succeeded":1})).unwrap()),
            ..Default::default()
        };
        assert_eq!(job_outcome(&job), None);
        job.status = Some(
            serde_json::from_value(json!({"conditions":[{"type":"Complete","status":"True"}]}))
                .unwrap(),
        );
        assert_eq!(job_outcome(&job), Some(true));
    }
}
