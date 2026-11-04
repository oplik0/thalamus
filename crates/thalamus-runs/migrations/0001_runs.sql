CREATE TABLE IF NOT EXISTS thalamus_runs (
    id UUID PRIMARY KEY,
    team TEXT NOT NULL,
    actor TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    request JSONB NOT NULL,
    job_name TEXT NOT NULL UNIQUE,
    job_uid TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'ingesting', 'ingestion_failed', 'succeeded', 'failed')),
    ownership_nonce UUID NOT NULL,
    job_spec JSONB,
    job_namespace TEXT,
    artifact JSONB,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (team, idempotency_key)
);
CREATE TABLE IF NOT EXISTS thalamus_run_audit (
    sequence BIGSERIAL PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES thalamus_runs(id),
    team TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail JSONB NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS thalamus_run_audit_run ON thalamus_run_audit (run_id, sequence);
CREATE OR REPLACE FUNCTION thalamus_runs_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id OR NEW.team IS DISTINCT FROM OLD.team OR
       NEW.actor IS DISTINCT FROM OLD.actor OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key OR
       NEW.fingerprint IS DISTINCT FROM OLD.fingerprint OR NEW.request IS DISTINCT FROM OLD.request OR
       NEW.job_name IS DISTINCT FROM OLD.job_name OR NEW.created_at IS DISTINCT FROM OLD.created_at OR
       (OLD.job_uid IS NOT NULL AND NEW.job_uid IS DISTINCT FROM OLD.job_uid) OR
       (OLD.artifact IS NOT NULL AND NEW.artifact IS DISTINCT FROM OLD.artifact) OR
       (OLD.job_spec IS NOT NULL AND NEW.job_spec IS DISTINCT FROM OLD.job_spec) OR
       (OLD.job_namespace IS NOT NULL AND NEW.job_namespace IS DISTINCT FROM OLD.job_namespace) OR
       NEW.ownership_nonce IS DISTINCT FROM OLD.ownership_nonce THEN
        RAISE EXCEPTION 'immutable run identity or artifact changed';
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS thalamus_runs_immutable_trigger ON thalamus_runs;
CREATE TRIGGER thalamus_runs_immutable_trigger BEFORE UPDATE ON thalamus_runs
    FOR EACH ROW EXECUTE FUNCTION thalamus_runs_immutable();
