-- Existing job keys were not bound to a deployment. Revoke them rather than
-- guessing their scope; their run IDs remain reserved against accidental reuse.
UPDATE identity_keys SET revoked_at = COALESCE(revoked_at, now()) WHERE kind = 'job';

ALTER TABLE identity_projects
    ADD COLUMN traffic_class TEXT NOT NULL DEFAULT 'standard'
    CHECK (traffic_class ~ '^[a-z0-9]([a-z0-9-]{0,62})$');
ALTER TABLE identity_budget_caps
    ADD COLUMN allowed_classes TEXT[] NOT NULL DEFAULT ARRAY['standard']::TEXT[]
    CHECK (cardinality(allowed_classes) > 0 AND array_position(allowed_classes, NULL) IS NULL AND 'standard' = ANY(allowed_classes));
-- The gateway rate-limit override is a 32-bit positive integer. Keep old
-- oversized rows readable until the next trusted refresh replaces them.
ALTER TABLE identity_budget_caps ADD CONSTRAINT identity_budget_cap_gateway_range
    CHECK (max_tokens <= 4294967295) NOT VALID;

ALTER TABLE identity_keys
    ADD COLUMN deployment TEXT,
    ADD COLUMN run_kind TEXT,
    ADD COLUMN request_budget BIGINT,
    ADD COLUMN debug_session_id UUID UNIQUE,
    ADD COLUMN debug_model TEXT,
    ADD COLUMN debug_target_service TEXT;
ALTER TABLE identity_keys DROP CONSTRAINT identity_keys_kind_check;
ALTER TABLE identity_keys DROP CONSTRAINT identity_run_key_scope;
ALTER TABLE identity_keys ADD CONSTRAINT identity_keys_kind_check
    CHECK (kind IN ('personal', 'project', 'job', 'debug'));
ALTER TABLE identity_keys ADD CONSTRAINT identity_key_binding_check CHECK (
    (kind IN ('personal', 'project') AND run_id IS NULL AND deployment IS NULL AND run_kind IS NULL AND request_budget IS NULL AND debug_session_id IS NULL AND debug_model IS NULL AND debug_target_service IS NULL)
    OR (kind = 'job' AND debug_session_id IS NULL AND debug_model IS NULL AND debug_target_service IS NULL AND
        ((revoked_at IS NOT NULL) OR
         (run_id IS NOT NULL AND deployment IS NOT NULL AND length(trim(deployment)) > 0 AND
          project_id IS NOT NULL AND expires_at IS NOT NULL AND cardinality(allowed_services) = 1 AND
          run_kind IS NOT NULL AND ((run_kind = 'validation' AND request_budget IS NULL) OR
          (run_kind = 'benchmark' AND request_budget IS NOT NULL AND request_budget BETWEEN 1 AND 1000000)))))
    OR (kind = 'debug' AND run_id IS NULL AND deployment IS NULL AND run_kind IS NULL AND request_budget IS NULL AND debug_session_id IS NOT NULL AND
        debug_model IS NOT NULL AND length(trim(debug_model)) > 0 AND
        debug_target_service IS NOT NULL AND length(trim(debug_target_service)) > 0 AND project_id IS NOT NULL AND
        expires_at IS NOT NULL AND cardinality(allowed_services) = 1 AND allowed_services[1] = debug_model)
);

-- A complete, trusted IdP snapshot is optional. Once configured, its
-- observation time fences old login claims and bounds projected key validity.
CREATE TABLE identity_idp_projection (
    issuer TEXT PRIMARY KEY,
    observed_at TIMESTAMPTZ NOT NULL,
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32)
);
