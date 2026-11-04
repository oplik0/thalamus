-- The runs schema must be installed first. Observations are append-only: retrying
-- an operation can return the original evidence, but cannot replace it.
CREATE TABLE IF NOT EXISTS thalamus_run_prelaunch_evidence (
    run_id UUID PRIMARY KEY REFERENCES thalamus_runs(id),
    request_fingerprint TEXT NOT NULL CHECK (request_fingerprint ~ '^[0-9a-f]{64}$'),
    snapshot JSONB NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS thalamus_run_ingestion_evidence (
    run_id UUID PRIMARY KEY REFERENCES thalamus_run_prelaunch_evidence(run_id),
    request_fingerprint TEXT NOT NULL CHECK (request_fingerprint ~ '^[0-9a-f]{64}$'),
    outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'failed')),
    snapshot JSONB NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    job_uid TEXT NOT NULL UNIQUE CHECK (length(trim(job_uid)) > 0),
    pod_uid TEXT NOT NULL UNIQUE CHECK (length(trim(pod_uid)) > 0),
    runner JSONB NOT NULL CHECK (jsonb_typeof(runner) = 'object'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (runner->>'job_uid' = job_uid),
    CHECK (runner->>'pod_uid' = pod_uid)
);

-- Once a completed Job was observed against different evidence, a later return
-- to the old configuration must not make that execution verifiable.
CREATE TABLE IF NOT EXISTS thalamus_run_evidence_rejections (
    run_id UUID PRIMARY KEY REFERENCES thalamus_run_prelaunch_evidence(run_id),
    request_fingerprint TEXT NOT NULL CHECK (request_fingerprint ~ '^[0-9a-f]{64}$'),
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    snapshot JSONB CHECK (snapshot IS NULL OR jsonb_typeof(snapshot) = 'object'),
    runner JSONB CHECK (runner IS NULL OR jsonb_typeof(runner) = 'object'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION thalamus_run_evidence_insert_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    run_row thalamus_runs%ROWTYPE;
    start_row thalamus_run_prelaunch_evidence%ROWTYPE;
    runner_count BIGINT;
    runner_image TEXT;
BEGIN
    -- The run row serializes evidence insertion with run state/Job identity changes.
    SELECT * INTO run_row FROM thalamus_runs WHERE id = NEW.run_id FOR UPDATE;
    IF NOT FOUND OR run_row.fingerprint IS DISTINCT FROM NEW.request_fingerprint THEN
        RAISE EXCEPTION 'run evidence fingerprint mismatch or missing run';
    END IF;
    IF TG_TABLE_NAME = 'thalamus_run_evidence_rejections' THEN
        IF run_row.state NOT IN ('ingesting', 'running') OR EXISTS
            (SELECT 1 FROM thalamus_run_ingestion_evidence WHERE run_id = NEW.run_id) THEN
            RAISE EXCEPTION 'cannot reject evidence at this stage';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.snapshot->>'service' IS DISTINCT FROM run_row.request->>'service' THEN
        RAISE EXCEPTION 'run evidence service mismatch';
    END IF;
    IF TG_TABLE_NAME = 'thalamus_run_prelaunch_evidence' THEN
        IF run_row.state <> 'pending' OR run_row.job_uid IS NOT NULL THEN
            RAISE EXCEPTION 'prelaunch evidence must precede Job recording';
        END IF;
        RETURN NEW;
    END IF;
    IF EXISTS (SELECT 1 FROM thalamus_run_evidence_rejections WHERE run_id = NEW.run_id) THEN
        RAISE EXCEPTION 'run evidence was previously rejected';
    END IF;

    SELECT * INTO start_row FROM thalamus_run_prelaunch_evidence WHERE run_id = NEW.run_id;
    IF NOT FOUND OR start_row.request_fingerprint IS DISTINCT FROM NEW.request_fingerprint OR
       (start_row.snapshot - 'observed_at') IS DISTINCT FROM (NEW.snapshot - 'observed_at') THEN
        RAISE EXCEPTION 'missing or changed prelaunch identity';
    END IF;
    IF (NEW.outcome = 'succeeded' AND run_row.state <> 'ingesting') OR
       (NEW.outcome = 'failed' AND run_row.state <> 'running') OR run_row.job_uid IS NULL OR
       length(trim(run_row.job_uid)) = 0 OR run_row.job_uid IS DISTINCT FROM NEW.job_uid OR
       NEW.runner->>'job' IS DISTINCT FROM run_row.job_name OR
       NEW.runner->>'job_uid' IS DISTINCT FROM NEW.job_uid OR
       NEW.runner->>'pod_uid' IS DISTINCT FROM NEW.pod_uid OR
       NEW.runner->>'pod_owner_job_uid' IS DISTINCT FROM NEW.job_uid OR
       COALESCE(length(trim(NEW.runner->>'pod')), 0) = 0 THEN
        RAISE EXCEPTION 'run Job identity or ingestion stage mismatch';
    END IF;
    SELECT count(*), min(container->>'image') INTO runner_count, runner_image
      FROM jsonb_array_elements(CASE WHEN jsonb_typeof(run_row.job_spec#>'{template,spec,containers}') = 'array'
                                    THEN run_row.job_spec#>'{template,spec,containers}' ELSE '[]'::jsonb END) AS containers(container)
     WHERE container->>'name' = 'runner';
    IF runner_count <> 1 OR runner_image IS NULL OR
       substring(runner_image FROM '@(sha256:[0-9a-f]{64})$') IS NULL OR
       NEW.runner->>'image' IS DISTINCT FROM runner_image OR
       NEW.runner->>'image_id' IS DISTINCT FROM substring(runner_image FROM '@(sha256:[0-9a-f]{64})$') THEN
        RAISE EXCEPTION 'runner image identity mismatch';
    END IF;
    RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION thalamus_run_evidence_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'run evidence is immutable';
END $$;

DROP TRIGGER IF EXISTS thalamus_prelaunch_insert_guard ON thalamus_run_prelaunch_evidence;
CREATE TRIGGER thalamus_prelaunch_insert_guard BEFORE INSERT ON thalamus_run_prelaunch_evidence
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_insert_guard();
DROP TRIGGER IF EXISTS thalamus_ingestion_insert_guard ON thalamus_run_ingestion_evidence;
CREATE TRIGGER thalamus_ingestion_insert_guard BEFORE INSERT ON thalamus_run_ingestion_evidence
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_insert_guard();
DROP TRIGGER IF EXISTS thalamus_rejection_insert_guard ON thalamus_run_evidence_rejections;
CREATE TRIGGER thalamus_rejection_insert_guard BEFORE INSERT ON thalamus_run_evidence_rejections
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_insert_guard();
DROP TRIGGER IF EXISTS thalamus_prelaunch_immutable ON thalamus_run_prelaunch_evidence;
CREATE TRIGGER thalamus_prelaunch_immutable BEFORE UPDATE OR DELETE ON thalamus_run_prelaunch_evidence
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_immutable();
DROP TRIGGER IF EXISTS thalamus_ingestion_immutable ON thalamus_run_ingestion_evidence;
CREATE TRIGGER thalamus_ingestion_immutable BEFORE UPDATE OR DELETE ON thalamus_run_ingestion_evidence
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_immutable();
DROP TRIGGER IF EXISTS thalamus_rejection_immutable ON thalamus_run_evidence_rejections;
CREATE TRIGGER thalamus_rejection_immutable BEFORE UPDATE OR DELETE ON thalamus_run_evidence_rejections
    FOR EACH ROW EXECUTE FUNCTION thalamus_run_evidence_immutable();
