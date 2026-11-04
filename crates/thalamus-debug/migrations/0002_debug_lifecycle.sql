-- Durable upstream prompt dispatch and collector fencing. Claims are never
-- automatically expired: OpenCode does not offer an idempotency key or a replay
-- cursor, so an uncertain dispatch must not be sent a second time.
CREATE TABLE IF NOT EXISTS thalamus_debug_prompts (
    session_id UUID NOT NULL REFERENCES thalamus_debug_sessions(id),
    kind TEXT NOT NULL CHECK (kind IN ('seed', 'reply')),
    client_request_key TEXT NOT NULL CHECK (octet_length(client_request_key) BETWEEN 1 AND 256 AND client_request_key ~ '^[!-~]+$'),
    text_digest BYTEA NOT NULL CHECK (octet_length(text_digest) = 32),
    claim_token UUID NOT NULL CHECK (claim_token <> '00000000-0000-0000-0000-000000000000'::uuid),
    state TEXT NOT NULL CHECK (state IN ('claimed', 'accepted', 'released')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (session_id, kind, client_request_key),
    CHECK (kind <> 'seed' OR client_request_key = 'seed')
);

-- Retain every token, including released ones, so a delayed completion from a
-- prior pre-dispatch attempt cannot authorize release of a later attempt.
CREATE TABLE IF NOT EXISTS thalamus_debug_prompt_attempts (
    claim_token UUID PRIMARY KEY CHECK (claim_token <> '00000000-0000-0000-0000-000000000000'::uuid),
    session_id UUID NOT NULL,
    kind TEXT NOT NULL,
    client_request_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (session_id, kind, client_request_key)
        REFERENCES thalamus_debug_prompts(session_id, kind, client_request_key)
);

ALTER TABLE thalamus_debug_sessions
    ADD COLUMN IF NOT EXISTS collector_generation BIGINT NOT NULL DEFAULT 0
        CHECK (collector_generation >= 0),
    ADD COLUMN IF NOT EXISTS collector_gap_generation BIGINT NOT NULL DEFAULT 0
        CHECK (collector_gap_generation >= 0);

CREATE INDEX IF NOT EXISTS thalamus_debug_collectors_to_recover
    ON thalamus_debug_sessions(created_at, id)
    WHERE collector_generation > collector_gap_generation;
CREATE INDEX IF NOT EXISTS thalamus_debug_initializations_to_recover
    ON thalamus_debug_sessions(created_at, id)
    WHERE initialization_claim IS NOT NULL AND upstream_session_id IS NULL;

-- Association marker for workload resources. Admission/RBAC, not this marker,
-- prevents untrusted principals from creating or mutating reserved resources.
ALTER TABLE thalamus_debug_sessions
    ADD COLUMN IF NOT EXISTS ownership_nonce UUID NOT NULL DEFAULT gen_random_uuid()
        CHECK (ownership_nonce <> '00000000-0000-0000-0000-000000000000'::uuid),
    ADD COLUMN IF NOT EXISTS job_uid TEXT
        CHECK (job_uid IS NULL OR (octet_length(job_uid) BETWEEN 1 AND 256 AND job_uid ~ '^[!-~]+$'));
CREATE UNIQUE INDEX IF NOT EXISTS thalamus_debug_ownership_nonce
    ON thalamus_debug_sessions(ownership_nonce);
CREATE UNIQUE INDEX IF NOT EXISTS thalamus_debug_job_uid
    ON thalamus_debug_sessions(job_uid) WHERE job_uid IS NOT NULL;

CREATE OR REPLACE FUNCTION thalamus_debug_preserve_resource_identity()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.ownership_nonce IS DISTINCT FROM OLD.ownership_nonce THEN
        RAISE EXCEPTION 'debug ownership nonce is immutable';
    END IF;
    IF OLD.job_uid IS NOT NULL AND NEW.job_uid IS DISTINCT FROM OLD.job_uid THEN
        RAISE EXCEPTION 'debug job UID is immutable once bound';
    END IF;
    RETURN NEW;
END;
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
        WHERE tgrelid = 'thalamus_debug_sessions'::regclass
          AND tgname = 'thalamus_debug_preserve_resource_identity'
          AND NOT tgisinternal
    ) THEN
        CREATE TRIGGER thalamus_debug_preserve_resource_identity
        BEFORE UPDATE OF ownership_nonce, job_uid ON thalamus_debug_sessions
        FOR EACH ROW EXECUTE FUNCTION thalamus_debug_preserve_resource_identity();
    END IF;
END;
$$;

-- A pre-lifecycle upstream may already have received a seed, even if no reply
-- or event was recorded. Never seed it again. The marker makes this cutover
-- safe when DebugStore::migrate is invoked more than once.
CREATE TABLE IF NOT EXISTS thalamus_debug_lifecycle_upgrade (
    applied BOOLEAN PRIMARY KEY CHECK (applied),
    applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
WITH first_apply AS (
    INSERT INTO thalamus_debug_lifecycle_upgrade(applied)
    VALUES (TRUE) ON CONFLICT DO NOTHING RETURNING applied
), legacy AS MATERIALIZED (
    SELECT id, next_event_sequence, upstream_session_id
    FROM thalamus_debug_sessions
    WHERE state = 'running'
      AND (upstream_session_id IS NOT NULL OR initialization_claim IS NOT NULL)
      AND EXISTS (SELECT 1 FROM first_apply)
    FOR UPDATE
), finalized AS (
    UPDATE thalamus_debug_sessions AS s
    SET state = 'failed',
        last_transition_from = 'running',
        outcome = '{"reason":"legacy_upstream_indeterminate"}'::jsonb,
        cleanup_required = TRUE,
        next_event_sequence = CASE
            WHEN legacy.next_event_sequence < 9223372036854775807
                THEN legacy.next_event_sequence + 1
            ELSE legacy.next_event_sequence
        END,
        updated_at = clock_timestamp()
    FROM legacy WHERE s.id = legacy.id
    RETURNING s.id, legacy.next_event_sequence AS event_sequence,
              legacy.upstream_session_id IS NULL AS unbound
), gap AS (
    INSERT INTO thalamus_debug_events(session_id, sequence, client_request_key, kind, payload)
    SELECT id, event_sequence,
           CASE WHEN unbound THEN 'collector.gap.initialization'
                ELSE 'collector.gap.legacy.' || id::text END,
           'transcript.gap', '{"reason":"legacy_upstream_indeterminate"}'::jsonb
    FROM finalized WHERE event_sequence < 9223372036854775807
    RETURNING session_id
)
UPDATE thalamus_debug_tool_credentials
SET revoked_at = clock_timestamp()
WHERE revoked_at IS NULL AND session_id IN (SELECT id FROM finalized);
