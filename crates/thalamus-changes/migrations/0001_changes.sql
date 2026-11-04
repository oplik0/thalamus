CREATE TABLE thalamus_changes (
    id UUID PRIMARY KEY,
    actor TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    request JSONB NOT NULL,
    state TEXT NOT NULL,
    branch TEXT NOT NULL,
    head_sha TEXT,
    pr_number BIGINT,
    pr_url TEXT,
    base_render_sha256 TEXT,
    proposed_render_sha256 TEXT,
    data_diff TEXT,
    rendered_diff TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (actor, idempotency_key)
);
CREATE TABLE thalamus_change_audit (
    id BIGSERIAL PRIMARY KEY,
    change_id UUID NOT NULL REFERENCES thalamus_changes(id),
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX thalamus_changes_pr_number ON thalamus_changes(pr_number);
