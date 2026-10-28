-- A rollout belongs to one originally requested change; stage changes are normal
-- GitOps PRs linked below. No serving resource is written by this controller.
CREATE TABLE IF NOT EXISTS thalamus_rollouts (
    change_id UUID PRIMARY KEY REFERENCES thalamus_changes(id),
    stage TEXT NOT NULL,
    prepared BOOLEAN NOT NULL DEFAULT FALSE,
    owner_team UUID,
    policy_digest TEXT,
    window_start TIMESTAMPTZ,
    window_end TIMESTAMPTZ,
    candidate_deployment TEXT,
    candidate_revision TEXT,
    old_deployment TEXT,
    in_place_pool BOOLEAN NOT NULL DEFAULT FALSE,
    previous_revision TEXT,
    workload_namespace TEXT,
    filler_verified_at TIMESTAMPTZ,
    candidate_validation_run UUID,
    public_validation_run UUID,
    queued BOOLEAN NOT NULL DEFAULT FALSE,
    queue_confirmed BOOLEAN NOT NULL DEFAULT FALSE,
    merge_requested_at TIMESTAMPTZ,
    merge_completed_at TIMESTAMPTZ,
    error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((window_start IS NULL AND window_end IS NULL) OR window_start < window_end)
);
CREATE TABLE IF NOT EXISTS thalamus_rollout_approvals (
    change_id UUID NOT NULL REFERENCES thalamus_rollouts(change_id),
    actor UUID NOT NULL,
    owner_team UUID NOT NULL,
    head_sha TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    window_start TIMESTAMPTZ NOT NULL,
    window_end TIMESTAMPTZ NOT NULL,
    policy_digest TEXT NOT NULL,
    approved_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (change_id, actor)
);
CREATE TABLE IF NOT EXISTS thalamus_rollout_stages (
    change_id UUID NOT NULL REFERENCES thalamus_rollouts(change_id),
    stage TEXT NOT NULL,
    stage_change_id UUID NOT NULL REFERENCES thalamus_changes(id),
    merged_commit TEXT,
    validation_run UUID,
    PRIMARY KEY (change_id, stage),
    UNIQUE (stage_change_id)
);
-- Existing proposals remain explicitly unprepared until revalidated; never
-- infer pool safety or approval for a previously published legacy PR.
INSERT INTO thalamus_rollouts(change_id,stage,window_start,window_end)
SELECT id,
    CASE request->'rollout'->>'mode' WHEN 'gated' THEN 'candidate_pr' WHEN 'in_place' THEN 'scheduled_pr' ELSE 'change_pr' END,
    CASE WHEN request->'rollout'->>'mode'='in_place' THEN (request->'rollout'->>'window_start')::timestamptz END,
    CASE WHEN request->'rollout'->>'mode'='in_place' THEN (request->'rollout'->>'window_end')::timestamptz END
FROM thalamus_changes WHERE true ON CONFLICT(change_id) DO NOTHING;
INSERT INTO thalamus_rollout_stages(change_id,stage,stage_change_id)
SELECT change_id,stage,change_id FROM thalamus_rollouts WHERE true
ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS thalamus_rollout_runs (
    run_id UUID PRIMARY KEY,
    change_id UUID NOT NULL REFERENCES thalamus_rollouts(change_id),
    stage TEXT NOT NULL CHECK(stage IN ('candidate','public')),
    owner_team UUID NOT NULL,
    project_id UUID NOT NULL,
    actor TEXT NOT NULL,
    service TEXT NOT NULL,
    deployment TEXT NOT NULL,
    revision TEXT NOT NULL,
    catalog_commit TEXT NOT NULL,
    old_deployment TEXT,
    UNIQUE(change_id,stage)
);
CREATE TABLE IF NOT EXISTS thalamus_candidate_runs (
    run_id UUID PRIMARY KEY,
    change_id UUID NOT NULL REFERENCES thalamus_rollouts(change_id),
    owner_team UUID NOT NULL,
    project_id UUID NOT NULL,
    service TEXT NOT NULL,
    deployment TEXT NOT NULL,
    revision TEXT NOT NULL,
    catalog_commit TEXT NOT NULL,
    old_deployment TEXT,
    kind TEXT NOT NULL CHECK(kind IN ('validation','benchmark'))
);
CREATE INDEX IF NOT EXISTS thalamus_rollouts_stage ON thalamus_rollouts(stage, updated_at);
CREATE TABLE IF NOT EXISTS thalamus_merge_groups (
    repository JSONB NOT NULL,
    base_ref TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    head_ref TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository, head_sha)
);
CREATE TABLE IF NOT EXISTS thalamus_ci_statuses (
    repository JSONB NOT NULL,
    sha TEXT NOT NULL,
    status_id BIGINT NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY (repository, sha)
);
CREATE OR REPLACE FUNCTION thalamus_rollout_runs_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'rollout validation bindings are immutable';
END;
$$;
DROP TRIGGER IF EXISTS thalamus_rollout_runs_immutable_trigger ON thalamus_rollout_runs;
CREATE TRIGGER thalamus_rollout_runs_immutable_trigger BEFORE UPDATE OR DELETE ON thalamus_rollout_runs
    FOR EACH ROW EXECUTE FUNCTION thalamus_rollout_runs_immutable();
DROP TRIGGER IF EXISTS thalamus_candidate_runs_immutable_trigger ON thalamus_candidate_runs;
CREATE TRIGGER thalamus_candidate_runs_immutable_trigger BEFORE UPDATE OR DELETE ON thalamus_candidate_runs
    FOR EACH ROW EXECUTE FUNCTION thalamus_rollout_runs_immutable();
